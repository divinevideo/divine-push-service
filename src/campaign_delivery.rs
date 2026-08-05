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
    config::CampaignDeliverySettings, error::Result, fcm_sender::FcmError, models::FcmPayload,
    redis_store, state::AppState,
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

fn refusal(delivery: &PendingDelivery, status: DeliveryStatus, reason: &str) -> DeliveryResult {
    DeliveryResult {
        idempotency_key: delivery.idempotency_key.clone(),
        status,
        reason: Some(reason.to_string()),
    }
}

/// The decisions that can be made before touching Redis or FCM.
///
/// Ordering is load-bearing: consent is refused before anything is looked up,
/// so an unconfigured deployment cannot leak the existence of a recipient by
/// behaving differently for one who has devices. Keeping these in one pure
/// function is what lets that order be asserted without a live Redis.
///
/// Returns the parsed recipient when delivery may proceed, or the refusal to
/// report back.
fn precheck(
    settings: &CampaignDeliverySettings,
    delivery: &PendingDelivery,
    now: i64,
) -> std::result::Result<PublicKey, DeliveryResult> {
    if !settings.allow_unverified_consent {
        // Marketing consent is not expressible in kind 3083 yet, and no
        // recipient timezone is stored, so quiet hours cannot be evaluated
        // either. Refusing is the only honest answer.
        return Err(refusal(
            delivery,
            DeliveryStatus::Suppressed,
            "consent_not_verifiable",
        ));
    }

    if is_expired(delivery.expires_at.as_deref(), now) {
        return Err(refusal(
            delivery,
            DeliveryStatus::Suppressed,
            "campaign_expired",
        ));
    }

    PublicKey::from_str(&delivery.recipient_pubkey).map_err(|_| {
        refusal(
            delivery,
            DeliveryStatus::PermanentFailure,
            "invalid_recipient_pubkey",
        )
    })
}

/// Delivers one campaign notification, or explains why it will not.
///
/// Order matters. Consent is refused before anything is looked up; see
/// [`precheck`].
pub async fn deliver(state: &AppState, delivery: &PendingDelivery, now: i64) -> DeliveryResult {
    let settings = &state.settings.campaign_delivery;

    let refuse = |status, reason: &str| refusal(delivery, status, reason);

    let recipient = match precheck(settings, delivery, now) {
        Ok(recipient) => recipient,
        Err(refused) => return refused,
    };

    // Final idempotency lives here, not in the campaign tool. A lease can
    // expire after we accepted a message but before the result was reported,
    // so the same key will legitimately be offered again.
    match redis_store::claim_campaign_delivery(
        &state.redis_pool,
        &dedup_key(&delivery.idempotency_key),
        settings.dedup_ttl_secs,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return refuse(DeliveryStatus::Suppressed, "already_delivered"),
        Err(e) => {
            error!(error = %e, "Failed to claim campaign delivery");
            return refuse(DeliveryStatus::RetryableFailure, "dedup_unavailable");
        }
    }

    let tokens = match redis_store::get_tokens_for_pubkey(&state.redis_pool, &recipient).await {
        Ok(tokens) => tokens,
        Err(e) => {
            error!(error = %e, "Failed to load device tokens");
            return refuse(DeliveryStatus::RetryableFailure, "token_lookup_failed");
        }
    };

    if tokens.is_empty() {
        return refuse(DeliveryStatus::PermanentFailure, "no_device");
    }

    let payload = campaign_payload(delivery, &recipient);
    let outcomes = state.fcm_client.send_batch(&tokens, payload).await;

    let mut accepted = false;
    let mut retryable = false;
    let mut rejected = false;
    let mut dropped_tokens = false;
    for (token, outcome) in outcomes {
        match outcome {
            Ok(()) => accepted = true,
            Err(FcmError::TokenNotRegistered) => {
                // The device is gone. Drop the token so the next campaign does
                // not pay for it again.
                dropped_tokens = true;
                if let Err(e) =
                    redis_store::remove_token(&state.redis_pool, &recipient, &token).await
                {
                    warn!(error = %e, key = %delivery.idempotency_key, "Failed to remove unregistered token");
                }
            }
            // FCM rejected the message itself. Re-sending identical content
            // cannot succeed, so reporting this retryable would have the
            // campaign tool re-offer it forever.
            Err(e @ FcmError::InvalidRequest(_)) => {
                warn!(error = %e, key = %delivery.idempotency_key, "Campaign push rejected as invalid");
                rejected = true;
            }
            Err(e) => {
                warn!(error = %e, key = %delivery.idempotency_key, "Campaign push failed for one token");
                retryable = true;
            }
        }
    }

    if accepted {
        DeliveryResult {
            idempotency_key: delivery.idempotency_key.clone(),
            status: DeliveryStatus::Delivered,
            reason: None,
        }
    } else if retryable {
        refuse(DeliveryStatus::RetryableFailure, "provider_error")
    } else if rejected {
        refuse(DeliveryStatus::PermanentFailure, "invalid_payload")
    } else if dropped_tokens {
        // Distinguished from "never had a device" so the campaign tool can
        // tell a recipient who never registered from one whose last device
        // has just been dropped.
        refuse(DeliveryStatus::PermanentFailure, "all_tokens_unregistered")
    } else {
        refuse(DeliveryStatus::PermanentFailure, "no_device")
    }
}

async fn poll_once(
    state: &AppState,
    http: &reqwest::Client,
    token: &CancellationToken,
) -> Result<usize> {
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
        return Err(crate::error::ServiceError::Internal(format!(
            "Pending delivery poll returned {}",
            response.status()
        )));
    }

    let pending: PendingResponse = response.json().await.map_err(|e| {
        crate::error::ServiceError::Internal(format!("Pending delivery decode failed: {e}"))
    })?;

    if pending.deliveries.is_empty() {
        return Ok(0);
    }

    let mut results = Vec::with_capacity(pending.deliveries.len());
    for delivery in &pending.deliveries {
        if token.is_cancelled() {
            // Stop claiming new work on shutdown. Whatever is already claimed
            // but unreported is re-offered once the lease expires.
            warn!(
                processed = results.len(),
                remaining = pending.deliveries.len() - results.len(),
                "Cancelled mid-batch. Reporting what was processed."
            );
            break;
        }
        // Recomputed per delivery rather than once per batch: a batch of
        // batch_size sends can outlive an expiry, and delivering after it has
        // passed is exactly what is_expired exists to prevent.
        let now = chrono::Utc::now().timestamp();
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

    // A failed report loses the outcome for every delivery in the batch. The
    // dedup key stops the re-offer becoming a second push, but it does not
    // preserve the outcome: each of these rows comes back and settles
    // "already_delivered", so a genuine delivery is recorded as suppressed and
    // a genuine retryable failure is never retried. Logged loudly for that
    // reason rather than because the push itself is at risk.
    match reported {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            error!(status = %response.status(), count, "Reporting delivery results failed. Outcomes for this batch are lost.")
        }
        Err(e) => {
            error!(error = %e, count, "Reporting delivery results failed. Outcomes for this batch are lost.")
        }
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
                match poll_once(&state, &http, &token).await {
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

    /// Consent allowed, so `precheck` turns on the remaining gates alone.
    fn consenting() -> CampaignDeliverySettings {
        CampaignDeliverySettings {
            allow_unverified_consent: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_consent_gate_suppresses_by_default() {
        // The single control between the campaign tool and every user's phone.
        let settings = CampaignDeliverySettings::default();
        assert!(
            !settings.allow_unverified_consent,
            "consent must default to refused"
        );

        let refused = precheck(&settings, &delivery(None), 1_800_000_000)
            .expect_err("delivery must be refused while consent is unverifiable");
        assert_eq!(refused.status, DeliveryStatus::Suppressed);
        assert_eq!(refused.reason.as_deref(), Some("consent_not_verifiable"));
    }

    #[test]
    fn test_consent_is_refused_before_the_recipient_is_even_parsed() {
        // The privacy claim: an unconfigured deployment must not behave
        // differently for a recipient who is valid, so the consent refusal has
        // to win over an unparseable pubkey.
        let mut invalid = delivery(None);
        invalid.recipient_pubkey = "not-a-pubkey".to_string();

        let refused = precheck(
            &CampaignDeliverySettings::default(),
            &invalid,
            1_800_000_000,
        )
        .expect_err("refused");
        assert_eq!(refused.reason.as_deref(), Some("consent_not_verifiable"));

        // With consent allowed, the same delivery reaches the pubkey check.
        let refused = precheck(&consenting(), &invalid, 1_800_000_000).expect_err("refused");
        assert_eq!(refused.status, DeliveryStatus::PermanentFailure);
        assert_eq!(refused.reason.as_deref(), Some("invalid_recipient_pubkey"));
    }

    #[test]
    fn test_consent_wins_over_expiry() {
        let refused = precheck(
            &CampaignDeliverySettings::default(),
            &delivery(Some("2020-01-01T00:00:00Z")),
            1_800_000_000,
        )
        .expect_err("refused");
        assert_eq!(refused.reason.as_deref(), Some("consent_not_verifiable"));
    }

    #[test]
    fn test_expired_campaign_is_suppressed_when_consent_is_allowed() {
        let refused = precheck(
            &consenting(),
            &delivery(Some("2020-01-01T00:00:00Z")),
            1_800_000_000,
        )
        .expect_err("refused");
        assert_eq!(refused.status, DeliveryStatus::Suppressed);
        assert_eq!(refused.reason.as_deref(), Some("campaign_expired"));
    }

    #[test]
    fn test_precheck_passes_a_live_delivery_through() {
        let recipient = precheck(&consenting(), &delivery(None), 1_800_000_000)
            .expect("a live delivery to a valid pubkey proceeds");
        assert_eq!(recipient.to_hex(), "a".repeat(64));
    }

    #[test]
    fn test_refusal_always_carries_the_idempotency_key() {
        // divine-engagement settles rows by this key; a refusal without one
        // cannot be recorded against anything.
        let refused =
            precheck(&CampaignDeliverySettings::default(), &delivery(None), 0).expect_err("refused");
        assert_eq!(refused.idempotency_key, "rev-1:abc");
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
    fn test_payload_is_data_only_and_carries_the_revision() {
        let recipient = Keys::generate().public_key();
        let payload = campaign_payload(&delivery(None), &recipient);

        assert!(payload.notification.is_none(), "must stay data-only");
        let data = payload.data.expect("data");
        assert_eq!(data.get("type").unwrap(), "campaign");
        assert_eq!(data.get("campaignRevisionId").unwrap(), "rev-1");
        assert_eq!(data.get("receiverPubkey").unwrap(), &recipient.to_hex());
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
            "recipientPubkey": "aaaa",
            "category": "engagement",
            "title": "t",
            "body": "b",
            "tapTarget": { "type": "app_route", "value": "/x" },
            "expiresAt": null
        }"#;
        let parsed: PendingDelivery = serde_json::from_str(json).expect("decodes");
        assert_eq!(parsed.idempotency_key, "rev-1:abc");
        assert_eq!(parsed.tap_target.target_type, "app_route");
        assert!(parsed.expires_at.is_none());
    }

    #[test]
    fn test_result_serialises_to_the_contract_statuses() {
        let encoded = serde_json::to_string(&DeliveryResult {
            idempotency_key: "k".to_string(),
            status: DeliveryStatus::RetryableFailure,
            reason: Some("provider_error".to_string()),
        })
        .unwrap();
        assert!(encoded.contains("\"retryable_failure\""));
        assert!(encoded.contains("\"idempotencyKey\":\"k\""));
    }
}
