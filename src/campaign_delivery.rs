//! Collects campaign notifications from `divine-engagement` and delivers them.
//!
//! `divine-engagement` runs on Cloudflare and this service runs on GKE with no
//! public ingress, so the handover is a pull rather than a push: we poll its
//! internal delivery API, authenticated with a Cloudflare Access service token,
//! and report outcomes back. The contract lives in
//! `divine-engagement/docs/push-service-contract.md`.
//!
//! This service remains the final enforcement point. The campaign tool decides
//! *what* to say and *to whom*; everything about whether a given person should
//! actually be interrupted is decided here.

use crate::{
    error::Result, fcm_sender::FcmError, models::FcmPayload, redis_store, state::AppState,
};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Debug, Deserialize)]
pub struct TapTarget {
    #[serde(rename = "type")]
    pub target_type: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDelivery {
    pub idempotency_key: String,
    pub campaign_revision_id: String,
    pub recipient_pubkey: String,
    pub category: String,
    pub title: String,
    pub body: String,
    pub tap_target: TapTarget,
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingResponse {
    deliveries: Vec<PendingDelivery>,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Delivered,
    Suppressed,
    PermanentFailure,
    RetryableFailure,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryResult {
    pub idempotency_key: String,
    pub status: DeliveryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResultsRequest {
    results: Vec<DeliveryResult>,
}

fn dedup_key(idempotency_key: &str) -> String {
    format!("campaign_delivery:{idempotency_key}")
}

/// How long a claim stands before a push has actually been accepted.
///
/// It only has to outlive `divine-engagement`'s lease (`leaseSeconds`, 300 at
/// the time of writing) so that a batch this process dies in the middle of is
/// re-offered and retried rather than suppressed as already delivered. That is
/// the deploy-time norm rather than an edge case: the binary is PID 1 in its
/// container and handles only SIGINT, so a pod termination is an ungraceful
/// kill. Not read from the poll response yet — `PendingResponse` does not
/// decode `leaseSeconds`.
const CLAIM_TTL_SECS: u64 = 600;

/// `divine-engagement`'s `LEASE_SECONDS`, which `CLAIM_TTL_SECS` must outlive.
///
/// Duplicated rather than read from the wire because `PendingResponse` does not
/// decode `leaseSeconds` yet. Tests assert the ordering so the two cannot drift
/// apart silently.
#[cfg(test)]
const ENGAGEMENT_LEASE_SECS: i64 = 300;

/// Drops a claim, so that whatever is re-offered gets a real second attempt.
///
/// Logged rather than propagated: the caller is already returning a result for
/// this delivery, and the short claim TTL bounds the damage if the drop does
/// not land.
async fn release_claim(state: &AppState, claim: &str, idempotency_key: &str) {
    if let Err(e) = redis_store::release_campaign_delivery(&state.redis_pool, claim).await {
        warn!(error = %e, key = %idempotency_key, "Failed to release campaign delivery claim");
    }
}

/// The error body, bounded for a log line.
///
/// `divine-engagement` distinguishes failures its status codes do not — three
/// different causes share a 403, and a 422 names the exact results row zod
/// rejected — so the body is the only artifact that says which one happened.
fn body_snippet(body: &str) -> String {
    body.chars().take(2048).collect()
}

/// Whether the campaign's own expiry has passed.
///
/// An expired campaign is dropped rather than delivered late. An unparseable
/// expiry counts as expired: a timestamp we cannot read is not permission.
fn is_expired(expires_at: Option<&str>, now: i64) -> bool {
    let Some(raw) = expires_at else {
        return false;
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(parsed) => parsed.timestamp() <= now,
        Err(_) => true,
    }
}

/// Builds the FCM payload for a campaign notification.
///
/// Data-only, matching the social notification path, so the client keeps
/// control of presentation. `campaignRevisionId` rather than a campaign id,
/// because the revision is what was approved and what the copy belongs to.
pub fn campaign_payload(delivery: &PendingDelivery, recipient: &PublicKey) -> FcmPayload {
    let mut data = HashMap::new();
    data.insert("type".to_string(), "campaign".to_string());
    data.insert("category".to_string(), delivery.category.clone());
    data.insert("title".to_string(), delivery.title.clone());
    data.insert("body".to_string(), delivery.body.clone());
    data.insert(
        "campaignRevisionId".to_string(),
        delivery.campaign_revision_id.clone(),
    );
    data.insert(
        "tapTargetType".to_string(),
        delivery.tap_target.target_type.clone(),
    );
    data.insert(
        "tapTargetValue".to_string(),
        delivery.tap_target.value.clone(),
    );
    data.insert("receiverPubkey".to_string(), recipient.to_hex());
    data.insert(
        "receiverNpub".to_string(),
        recipient.to_bech32().unwrap_or_default(),
    );

    FcmPayload {
        notification: None,
        data: Some(data),
        android: None,
        webpush: None,
        apns: None,
    }
}

/// Delivers one campaign notification, or explains why it will not.
///
/// Order matters. Consent is refused before anything is looked up, so an
/// unconfigured deployment cannot leak the existence of a recipient by
/// behaving differently for one who has devices.
pub async fn deliver(state: &AppState, delivery: &PendingDelivery, now: i64) -> DeliveryResult {
    let settings = &state.settings.campaign_delivery;

    let refuse = |status, reason: &str| DeliveryResult {
        idempotency_key: delivery.idempotency_key.clone(),
        status,
        reason: Some(reason.to_string()),
    };

    if !settings.allow_unverified_consent {
        // Marketing consent is not expressible in kind 3083 yet, and no
        // recipient timezone is stored, so quiet hours cannot be evaluated
        // either. Refusing is the only honest answer.
        return refuse(DeliveryStatus::Suppressed, "consent_not_verifiable");
    }

    if is_expired(delivery.expires_at.as_deref(), now) {
        return refuse(DeliveryStatus::Suppressed, "campaign_expired");
    }

    let Ok(recipient) = PublicKey::from_str(&delivery.recipient_pubkey) else {
        return refuse(DeliveryStatus::PermanentFailure, "invalid_recipient_pubkey");
    };

    // Final idempotency lives here, not in the campaign tool. A lease can
    // expire after we accepted a message but before the result was reported,
    // so the same key will legitimately be offered again.
    //
    // The claim survives only a delivery. It is taken for `CLAIM_TTL_SECS` and
    // promoted to the full dedup window once FCM has accepted a push; every
    // path that ends without one drops it. Holding the full window across a
    // failure would suppress the very retry this function asks for, and the
    // row would settle `already_delivered` for a push that never landed.
    let claim = dedup_key(&delivery.idempotency_key);
    match redis_store::claim_campaign_delivery(&state.redis_pool, &claim, CLAIM_TTL_SECS).await {
        Ok(true) => {}
        Ok(false) => return refuse(DeliveryStatus::Suppressed, "already_delivered"),
        Err(e) => {
            error!(error = %e, key = %delivery.idempotency_key, "Failed to claim campaign delivery");
            return refuse(DeliveryStatus::RetryableFailure, "dedup_unavailable");
        }
    }

    let tokens = match redis_store::get_tokens_for_pubkey(&state.redis_pool, &recipient).await {
        Ok(tokens) => tokens,
        Err(e) => {
            error!(error = %e, key = %delivery.idempotency_key, "Failed to load device tokens");
            release_claim(state, &claim, &delivery.idempotency_key).await;
            return refuse(DeliveryStatus::RetryableFailure, "token_lookup_failed");
        }
    };

    if tokens.is_empty() {
        release_claim(state, &claim, &delivery.idempotency_key).await;
        return refuse(DeliveryStatus::PermanentFailure, "no_device");
    }

    let payload = campaign_payload(delivery, &recipient);
    let outcomes = state.fcm_client.send_batch(&tokens, payload).await;

    let mut accepted = false;
    let mut retryable = false;
    for (token, outcome) in outcomes {
        match outcome {
            Ok(()) => accepted = true,
            Err(FcmError::TokenNotRegistered) => {
                // The device is gone. Drop the token so the next campaign does
                // not pay for it again.
                if let Err(e) =
                    redis_store::remove_token(&state.redis_pool, &recipient, &token).await
                {
                    warn!(error = %e, key = %delivery.idempotency_key, "Failed to remove unregistered token");
                }
            }
            Err(e) => {
                warn!(error = %e, key = %delivery.idempotency_key, "Campaign push failed for one token");
                retryable = true;
            }
        }
    }

    if accepted {
        // A push landed, so the claim becomes the durable record of it and has
        // to outlive any re-offer.
        match redis_store::promote_campaign_delivery(
            &state.redis_pool,
            &claim,
            settings.dedup_ttl_secs,
        )
        .await
        {
            Ok(true) => {}
            // The send outlived the claim, so a re-offer of this key can push a
            // second time. Both halves of that are the lease budget going
            // unread; the claim window is only standing in for it.
            Ok(false) => {
                warn!(key = %delivery.idempotency_key, "Campaign delivery claim expired mid-send")
            }
            Err(e) => {
                warn!(error = %e, key = %delivery.idempotency_key, "Failed to extend campaign delivery claim")
            }
        }
        DeliveryResult {
            idempotency_key: delivery.idempotency_key.clone(),
            status: DeliveryStatus::Delivered,
            reason: None,
        }
    } else if retryable {
        release_claim(state, &claim, &delivery.idempotency_key).await;
        refuse(DeliveryStatus::RetryableFailure, "provider_error")
    } else {
        release_claim(state, &claim, &delivery.idempotency_key).await;
        refuse(DeliveryStatus::PermanentFailure, "no_device")
    }
}

async fn poll_once(state: &AppState, http: &reqwest::Client) -> Result<usize> {
    let settings = &state.settings.campaign_delivery;
    let pending_url = format!(
        "{}/api/internal/deliveries/pending?limit={}",
        settings.api_base_url.trim_end_matches('/'),
        settings.batch_size
    );

    let response = http
        .get(&pending_url)
        .header("CF-Access-Client-Id", &settings.access_client_id)
        .header("CF-Access-Client-Secret", &settings.access_client_secret)
        .send()
        .await
        .map_err(|e| {
            crate::error::ServiceError::Internal(format!("Pending delivery poll failed: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(crate::error::ServiceError::Internal(format!(
            "Pending delivery poll returned {status}: {}",
            body_snippet(&body)
        )));
    }

    let pending: PendingResponse = response.json().await.map_err(|e| {
        crate::error::ServiceError::Internal(format!("Pending delivery decode failed: {e}"))
    })?;

    if pending.deliveries.is_empty() {
        return Ok(0);
    }

    let now = chrono::Utc::now().timestamp();
    let mut results = Vec::with_capacity(pending.deliveries.len());
    for delivery in &pending.deliveries {
        results.push(deliver(state, delivery, now).await);
    }

    let count = results.len();
    let results_url = format!(
        "{}/api/internal/deliveries/results",
        settings.api_base_url.trim_end_matches('/')
    );
    let reported = http
        .post(&results_url)
        .header("CF-Access-Client-Id", &settings.access_client_id)
        .header("CF-Access-Client-Secret", &settings.access_client_secret)
        .json(&ResultsRequest { results })
        .send()
        .await;

    match reported {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!(status = %status, body = %body_snippet(&body), "Reporting delivery results failed");
        }
        // Not fatal. The lease expires and the work is offered again, and the
        // dedup key above stops that becoming a second push.
        Err(e) => warn!(error = %e, "Reporting delivery results failed"),
    }

    Ok(count)
}

pub async fn run_campaign_delivery_service(
    state: Arc<AppState>,
    token: CancellationToken,
) -> Result<()> {
    let settings = state.settings.campaign_delivery.clone();

    if !settings.enabled {
        info!("Campaign delivery collection is disabled.");
        return Ok(());
    }
    if settings.api_base_url.is_empty()
        || settings.access_client_id.is_empty()
        || settings.access_client_secret.is_empty()
    {
        error!("Campaign delivery is enabled but not configured. Not polling.");
        return Ok(());
    }
    if !settings.allow_unverified_consent {
        warn!(
            "Campaign delivery is polling, but consent and quiet hours cannot be verified, so \
             every delivery will be suppressed. This is deliberate until preferences carry a \
             marketing category and registrations carry a timezone."
        );
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| crate::error::ServiceError::Internal(format!("HTTP client: {e}")))?;

    let mut ticker = interval(Duration::from_secs(settings.poll_interval_secs.max(1)));
    info!(
        interval_secs = settings.poll_interval_secs,
        batch_size = settings.batch_size,
        "Starting campaign delivery collection."
    );

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Campaign delivery collection cancelled. Shutting down...");
                break;
            }
            _ = ticker.tick() => {
                match poll_once(&state, &http).await {
                    Ok(0) => debug!("No campaign deliveries pending."),
                    Ok(count) => info!(count, "Processed campaign deliveries."),
                    Err(e) => error!(error = %e, "Campaign delivery poll failed."),
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fcm_sender::{FcmClient, FcmError, MockFcmSender};

    /// A Redis pool, or `None` when no Redis is reachable and none was asked for.
    ///
    /// Skipping is silent, and these tests assert the claim lifecycle, so a
    /// skipped run reports green having checked nothing. When `REDIS_URL` is
    /// set the operator has asked for a specific Redis, and failing to reach it
    /// is a broken run rather than a reason to skip.
    async fn test_redis_pool() -> Option<redis_store::RedisPool> {
        let explicit = std::env::var("REDIS_URL").ok();
        let redis_url = explicit
            .clone()
            .unwrap_or_else(|| "redis://localhost:6379".to_string());
        let reached = async {
            let pool = redis_store::create_pool(&redis_url, 5).await.ok()?;
            let mut conn = pool.get().await.ok()?;
            let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
            drop(conn);
            pong.ok().map(|_| pool)
        }
        .await;

        if reached.is_none() && explicit.is_some() {
            panic!("REDIS_URL={redis_url} was set but could not be reached; refusing to skip");
        }
        reached
    }

    /// State whose consent gate is open, so `deliver` reaches the claim.
    fn sending_state(pool: redis_store::RedisPool, sender: MockFcmSender) -> AppState {
        let mut settings = crate::config::Settings::new().unwrap();
        settings.campaign_delivery.allow_unverified_consent = true;
        AppState {
            settings,
            redis_pool: pool,
            fcm_client: Arc::new(FcmClient::new_with_impl(Box::new(sender))),
            service_keys: None,
            crypto_service: None,
            nostr_client: Arc::new(Client::default()),
            profile_client: Arc::new(Client::default()),
            mention_parser_service: None,
        }
    }

    /// Redis `TTL`: -2 when the key is gone, -1 when it has no expiry.
    async fn claim_ttl(pool: &redis_store::RedisPool, key: &str) -> i64 {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("TTL")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .unwrap()
    }

    /// An FCM stub that records the claim's TTL at the moment of the send.
    ///
    /// The claim is short-lived only while it is in flight, and no path leaves
    /// it observable in that state afterwards, so it has to be read from here.
    struct ClaimProbe {
        pool: redis_store::RedisPool,
        claim: String,
        ttl_at_send: Arc<std::sync::Mutex<Option<i64>>>,
    }

    #[async_trait::async_trait]
    impl crate::fcm_sender::FcmSend for ClaimProbe {
        async fn send_single(
            &self,
            _token: &str,
            _payload: FcmPayload,
        ) -> std::result::Result<(), FcmError> {
            *self.ttl_at_send.lock().unwrap() = Some(claim_ttl(&self.pool, &self.claim).await);
            Ok(())
        }
    }

    /// A delivery addressed to a real, registered recipient.
    async fn registered_delivery(
        pool: &redis_store::RedisPool,
        label: &str,
    ) -> (PendingDelivery, String) {
        let recipient = Keys::generate();
        let token = format!("campaign-{label}-{}", recipient.public_key().to_hex());
        redis_store::add_or_update_token(pool, &recipient.public_key(), &token)
            .await
            .unwrap();

        let mut pending = delivery(None);
        pending.idempotency_key = format!("rev-{label}:{}", recipient.public_key().to_hex());
        pending.recipient_pubkey = recipient.public_key().to_hex();
        (pending, token)
    }

    fn delivery(expires_at: Option<&str>) -> PendingDelivery {
        PendingDelivery {
            idempotency_key: "rev-1:abc".to_string(),
            campaign_revision_id: "rev-1".to_string(),
            recipient_pubkey: "a".repeat(64),
            category: "engagement".to_string(),
            title: "Three creators you follow posted".to_string(),
            body: "See what they made this week.".to_string(),
            tap_target: TapTarget {
                target_type: "app_route".to_string(),
                value: "/following/new".to_string(),
            },
            expires_at: expires_at.map(str::to_string),
        }
    }

    #[test]
    fn test_no_expiry_is_not_expired() {
        assert!(!is_expired(None, 1_800_000_000));
    }

    #[test]
    fn test_future_expiry_is_live() {
        assert!(!is_expired(Some("2030-01-01T00:00:00Z"), 1_800_000_000));
    }

    #[test]
    fn test_past_expiry_is_expired() {
        assert!(is_expired(Some("2020-01-01T00:00:00Z"), 1_800_000_000));
    }

    #[test]
    fn test_unparseable_expiry_counts_as_expired() {
        // A timestamp we cannot read is not permission to send.
        assert!(is_expired(Some("whenever"), 1_800_000_000));
    }

    #[test]
    fn test_payload_is_data_only_and_carries_every_delivery_value() {
        // Whole-map equality, not spot checks. The key-set test below pins the
        // shape; this pins which delivery field feeds which key, so a
        // wrong-source insert — the tap type where the tap value goes, title
        // and body swapped — cannot ship while the key set stays right.
        let recipient = Keys::generate().public_key();
        let payload = campaign_payload(&delivery(None), &recipient);

        assert!(payload.notification.is_none(), "must stay data-only");
        let data = payload.data.expect("data");

        let expected: HashMap<String, String> = [
            ("type", "campaign".to_string()),
            ("category", "engagement".to_string()),
            ("title", "Three creators you follow posted".to_string()),
            ("body", "See what they made this week.".to_string()),
            ("campaignRevisionId", "rev-1".to_string()),
            ("tapTargetType", "app_route".to_string()),
            ("tapTargetValue", "/following/new".to_string()),
            ("receiverPubkey", recipient.to_hex()),
            ("receiverNpub", recipient.to_bech32().expect("npub")),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();

        assert_eq!(data, expected);
    }

    #[test]
    fn test_payload_carries_exactly_the_expected_keys() {
        // Asserting the exact key set, rather than searching the encoded form
        // for the substring "token". Real FCM registration tokens do not
        // contain that word, so a substring check cannot detect one; this can
        // only pass while the map is exactly these keys.
        let recipient = Keys::generate().public_key();
        let payload = campaign_payload(&delivery(None), &recipient);

        assert!(payload.notification.is_none(), "must stay data-only");
        let data = payload.data.expect("data");

        let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "body",
                "campaignRevisionId",
                "category",
                "receiverNpub",
                "receiverPubkey",
                "tapTargetType",
                "tapTargetValue",
                "title",
                "type",
            ],
            "campaign payload key set changed; confirm no device token or other \
             sensitive field was introduced before updating this list"
        );
    }

    #[test]
    fn test_pending_delivery_decodes_the_contract_shape() {
        let json = r#"{
            "idempotencyKey": "rev-1:abc",
            "campaignRevisionId": "rev-1",
            "recipientPubkey": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "category": "engagement",
            "title": "t",
            "body": "b",
            "tapTarget": { "type": "app_route", "value": "/x" },
            "expiresAt": null
        }"#;
        let parsed: PendingDelivery = serde_json::from_str(json).expect("decodes");
        assert_eq!(parsed.idempotency_key, "rev-1:abc");
        assert_eq!(parsed.tap_target.target_type, "app_route");
        assert!(PublicKey::from_str(&parsed.recipient_pubkey).is_ok());
        assert!(parsed.expires_at.is_none());
    }

    #[test]
    fn test_pending_envelope_decodes_what_engagement_serves() {
        // The wire body is the envelope, not a bare delivery: engagement
        // unconditionally returns {"deliveries": [...], "leaseSeconds": 300}.
        // Nothing decodes leaseSeconds yet, so this pins that the extra field
        // does not break the envelope and that `deliveries` keeps its wire
        // name — a deny_unknown_fields or rename here would fail every
        // production poll while the bare-delivery fixtures below stayed green.
        let json = r#"{
            "deliveries": [{
                "idempotencyKey": "rev-1:abc",
                "campaignRevisionId": "rev-1",
                "recipientPubkey": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "category": "engagement",
                "title": "t",
                "body": "b",
                "tapTarget": { "type": "app_route", "value": "/x" },
                "expiresAt": "2026-08-05T12:00:00Z"
            }],
            "leaseSeconds": 300
        }"#;
        let parsed: PendingResponse = serde_json::from_str(json).expect("decodes");
        assert_eq!(parsed.deliveries.len(), 1);
        assert_eq!(parsed.deliveries[0].idempotency_key, "rev-1:abc");
        assert_eq!(
            parsed.deliveries[0].expires_at.as_deref(),
            Some("2026-08-05T12:00:00Z")
        );
    }

    #[test]
    fn test_decoded_expiry_survives_serde() {
        // The expiry tests above feed `is_expired` hand-written literals and the
        // decode test only ever sees `null`, so nothing carried a real
        // `expiresAt` across serde into `is_expired`. This does not reach the
        // gate in `deliver()`, which is still uncovered. Without this, dropping the
        // field from deserialization makes every campaign never-expiring and
        // the whole suite stays green.
        let json = r#"{
            "idempotencyKey": "rev-1:abc",
            "campaignRevisionId": "rev-1",
            "recipientPubkey": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "category": "engagement",
            "title": "t",
            "body": "b",
            "tapTarget": { "type": "app_route", "value": "/x" },
            "expiresAt": "2026-08-05T12:00:00Z"
        }"#;
        let parsed: PendingDelivery = serde_json::from_str(json).expect("decodes");
        assert_eq!(parsed.expires_at.as_deref(), Some("2026-08-05T12:00:00Z"));

        // 2026-08-05T12:00:00Z is 1785931200.
        assert!(!is_expired(parsed.expires_at.as_deref(), 1_785_931_199));
        // The campaign expires on the instant, not after it.
        assert!(is_expired(parsed.expires_at.as_deref(), 1_785_931_200));
        assert!(is_expired(parsed.expires_at.as_deref(), 1_785_931_201));
    }

    #[test]
    fn test_result_serialises_to_the_contract_statuses() {
        // divine-engagement parses `status` as a closed enum over the whole
        // results array, so one off-contract string rejects the entire batch.
        // Every variant needs pinning, not just the one this test used to cover.
        for (status, wire) in [
            (DeliveryStatus::Delivered, "delivered"),
            (DeliveryStatus::Suppressed, "suppressed"),
            (DeliveryStatus::PermanentFailure, "permanent_failure"),
            (DeliveryStatus::RetryableFailure, "retryable_failure"),
        ] {
            let encoded = serde_json::to_string(&DeliveryResult {
                idempotency_key: "k".to_string(),
                status,
                reason: None,
            })
            .unwrap();
            assert!(
                encoded.contains(&format!("\"status\":\"{wire}\"")),
                "{status:?} must serialise as {wire}, got {encoded}"
            );
            assert!(encoded.contains("\"idempotencyKey\":\"k\""));
            // `reason: None` must be omitted, not emitted as null.
            assert!(!encoded.contains("reason"));
        }
    }

    #[test]
    fn test_results_request_is_the_envelope_the_contract_expects() {
        // This is the struct poll_once actually puts on the wire.
        let encoded = serde_json::to_string(&ResultsRequest {
            results: vec![DeliveryResult {
                idempotency_key: "rev-1:abc".to_string(),
                status: DeliveryStatus::RetryableFailure,
                reason: Some("provider_error".to_string()),
            }],
        })
        .unwrap();
        assert_eq!(
            encoded,
            r#"{"results":[{"idempotencyKey":"rev-1:abc","status":"retryable_failure","reason":"provider_error"}]}"#
        );
    }

    #[tokio::test]
    async fn test_a_failed_send_releases_the_claim_so_the_re_offer_delivers() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, token) = registered_delivery(&pool, "retry").await;
        let claim = dedup_key(&pending.idempotency_key);

        // Attempt one: FCM refuses, so this reports retryable_failure and
        // divine-engagement re-offers the row once the lease expires.
        let failing = MockFcmSender::new();
        failing.set_error_for_token(&token, FcmError::InternalError);
        let first = deliver(
            &sending_state(pool.clone(), failing),
            &pending,
            1_800_000_000,
        )
        .await;
        assert_eq!(first.status, DeliveryStatus::RetryableFailure);
        assert_eq!(first.reason.as_deref(), Some("provider_error"));
        assert_eq!(
            claim_ttl(&pool, &claim).await,
            -2,
            "a retryable failure must leave no claim behind"
        );

        // Attempt two, the re-offer: it has to actually send. Holding the claim
        // here is what silently loses the push and records it as delivered.
        let working = MockFcmSender::new();
        let second = deliver(
            &sending_state(pool.clone(), working.clone()),
            &pending,
            1_800_000_000,
        )
        .await;
        assert_eq!(
            second.status,
            DeliveryStatus::Delivered,
            "the re-offered delivery must send, not settle already_delivered"
        );
        assert_eq!(
            working.get_sent_messages().len(),
            1,
            "the retry must reach FCM"
        );

        redis_store::release_campaign_delivery(&pool, &claim)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_delivered_claim_outlives_the_lease_window() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "promote").await;
        let claim = dedup_key(&pending.idempotency_key);

        let state = sending_state(pool.clone(), MockFcmSender::new());
        let result = deliver(&state, &pending, 1_800_000_000).await;
        assert_eq!(result.status, DeliveryStatus::Delivered);

        // Claimed for the lease window, promoted to the dedup window on send.
        // Left at the lease window, a re-offer after 600s would push twice.
        // Pinned to the configured window, not merely "longer than the claim".
        // Promoting to CLAIM_TTL_SECS + 1 is three orders of magnitude short of
        // the dedup window and would otherwise pass.
        let dedup_ttl = state.settings.campaign_delivery.dedup_ttl_secs as i64;
        let ttl = claim_ttl(&pool, &claim).await;
        assert!(
            (ttl - dedup_ttl).abs() <= 5,
            "a delivered push must hold its claim for the {dedup_ttl}s dedup window; TTL was {ttl}"
        );

        redis_store::release_campaign_delivery(&pool, &claim)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_an_in_flight_claim_only_outlives_the_lease() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "inflight").await;
        let claim = dedup_key(&pending.idempotency_key);

        let ttl_at_send = Arc::new(std::sync::Mutex::new(None));
        let probe = ClaimProbe {
            pool: pool.clone(),
            claim: claim.clone(),
            ttl_at_send: ttl_at_send.clone(),
        };
        let mut settings = crate::config::Settings::new().unwrap();
        settings.campaign_delivery.allow_unverified_consent = true;
        let dedup_ttl = settings.campaign_delivery.dedup_ttl_secs as i64;
        let state = AppState {
            settings,
            redis_pool: pool.clone(),
            fcm_client: Arc::new(FcmClient::new_with_impl(Box::new(probe))),
            service_keys: None,
            crypto_service: None,
            nostr_client: Arc::new(Client::default()),
            profile_client: Arc::new(Client::default()),
            mention_parser_service: None,
        };

        deliver(&state, &pending, 1_800_000_000).await;

        // Claimed for the lease window, not the dedup window. A process killed
        // between claim and report — the deploy-time norm here — has to expire
        // and let the re-offer through, not suppress it for seven days.
        let observed = ttl_at_send.lock().unwrap().expect("the probe must be hit");
        // Both bounds matter. Upper: not the dedup window, or a killed
        // process suppresses the re-offer for seven days. Lower: the claim has
        // to outlive divine-engagement's 300s lease, or it expires mid-send and
        // every delivery becomes double-pushable.
        assert!(
            observed <= CLAIM_TTL_SECS as i64 && observed < dedup_ttl,
            "an in-flight claim must be held for the lease window, not the dedup window; TTL was {observed}"
        );
        assert!(
            observed > ENGAGEMENT_LEASE_SECS,
            "an in-flight claim must outlive the {ENGAGEMENT_LEASE_SECS}s lease; TTL was {observed}"
        );

        redis_store::release_campaign_delivery(&pool, &claim)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_delivered_key_offered_again_is_suppressed() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "dedup").await;
        let claim = dedup_key(&pending.idempotency_key);

        let first = deliver(
            &sending_state(pool.clone(), MockFcmSender::new()),
            &pending,
            1_800_000_000,
        )
        .await;
        assert_eq!(first.status, DeliveryStatus::Delivered);

        // The point of claiming at all: a lease re-offered after a successful
        // push must not become a second one.
        let repeat = MockFcmSender::new();
        let second = deliver(
            &sending_state(pool.clone(), repeat.clone()),
            &pending,
            1_800_000_000,
        )
        .await;
        assert_eq!(second.status, DeliveryStatus::Suppressed);
        assert_eq!(second.reason.as_deref(), Some("already_delivered"));
        assert!(
            repeat.get_sent_messages().is_empty(),
            "a re-offered delivered key must not reach FCM again"
        );

        redis_store::release_campaign_delivery(&pool, &claim)
            .await
            .unwrap();
    }
}
