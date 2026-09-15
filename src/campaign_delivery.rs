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
    error::Result, fcm_sender::FcmError, models::FcmPayload, preferences, redis_store,
    state::AppState,
};
use futures_util::StreamExt;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Debug, Deserialize)]
struct TapTarget {
    #[serde(rename = "type")]
    target_type: String,
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingDelivery {
    idempotency_key: String,
    campaign_revision_id: String,
    recipient_pubkey: String,
    category: String,
    title: String,
    body: String,
    tap_target: TapTarget,
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingResponse {
    deliveries: Vec<PendingDelivery>,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum DeliveryStatus {
    Delivered,
    Deferred,
    Suppressed,
    PermanentFailure,
    RetryableFailure,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryResult {
    idempotency_key: String,
    status: DeliveryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<String>,
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
///
/// TODO(#41): replace with the lease budget the delivery API returns.
const CLAIM_TTL_SECS: u64 = 600;
const MAX_PENDING_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// `divine-engagement`'s `LEASE_SECONDS`, which `CLAIM_TTL_SECS` must outlive.
///
/// Duplicated rather than read from the wire because `PendingResponse` does not
/// decode `leaseSeconds` yet. Tests assert the ordering so the two cannot drift
/// apart silently.
#[cfg(test)]
const ENGAGEMENT_LEASE_SECS: i64 = 300;

/// Drops a claim, so that whatever is re-offered gets a real second attempt.
///
/// Logged rather than propagated, because the caller is already returning a
/// result for this delivery and has nothing better to do with the error.
///
/// That is a genuine hole rather than a bounded one, and the claim window does
/// not close it: a `retryable_failure` returns the row to `pending_delivery`
/// with its lease cleared, and `divine-engagement` re-offers pending rows with
/// no lease cutoff, so the next poll — 30 seconds later, not 600 — meets the
/// claim this call failed to drop and settles the row `already_delivered`. The
/// realistic instance is the `token_lookup_failed` path, where the lookup and
/// this release share a Redis pool and fail together. Closing it needs the
/// claim window tied to the lease budget rather than to a constant; see #41.
async fn release_claim(state: &AppState, claim: &str, owner: &str, idempotency_key: &str) {
    match redis_store::release_campaign_delivery(&state.redis_pool, claim, owner).await {
        Ok(true) => {}
        // Nothing was dropped because the key was no longer this attempt's.
        // The send outlived its own claim and the key now belongs to the
        // re-offer that produced it. Worth a line: it is the only place that
        // becomes visible.
        Ok(false) => {
            warn!(key = %idempotency_key, "Campaign delivery claim was no longer ours to release")
        }
        Err(e) => {
            warn!(error = %e, key = %idempotency_key, "Failed to release campaign delivery claim")
        }
    }
}

/// Counts a device token dropped because FCM said it was gone.
///
/// Only a confirmed removal counts. `remove_token` returns false when the token
/// had already been dropped or belonged to another pubkey, and counting those
/// would inflate the metric with no-ops. Same reason label as the social path,
/// so `push_tokens_pruned_total{reason="invalid"}` stays one number across both
/// delivery paths rather than silently omitting campaigns.
fn record_invalid_token_pruned(removed: bool) {
    if removed {
        crate::metrics::tokens_pruned("invalid", 1);
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

async fn bounded_response_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            crate::error::ServiceError::Internal(format!("Campaign API body read failed: {e}"))
        })?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(crate::error::ServiceError::Internal(format!(
                "Campaign API response exceeded {limit} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Whether the campaign's own expiry has passed.
///
/// An expired campaign is dropped rather than delivered late. An unparseable
/// expiry counts as expired: a timestamp we cannot read is not permission.
fn is_expired(expires_at: Option<&str>, now: i64) -> bool {
    let Some(raw) = expires_at else {
        return false;
    };
    let normalized;
    let candidate = if raw.len() == 17 && raw.ends_with('Z') {
        normalized = format!("{}:00Z", &raw[..raw.len() - 1]);
        normalized.as_str()
    } else {
        raw
    };
    match chrono::DateTime::parse_from_rfc3339(candidate) {
        Ok(parsed) => parsed.timestamp() <= now,
        Err(_) => true,
    }
}

fn validate_api_base_url(raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw).map_err(|e| {
        crate::error::ServiceError::Internal(format!("Invalid campaign API URL: {e}"))
    })?;
    let valid = url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/");
    if !valid {
        return Err(crate::error::ServiceError::Internal(
            "Campaign API base URL must be a credential-free HTTPS origin".to_string(),
        ));
    }
    Ok(())
}

/// Returns the next UTC instant outside quiet hours, or `None` when delivery
/// is currently permitted for this device offset.
///
/// The result comes back with a `Z` suffix, not the `+00:00` an offset-based
/// RFC 3339 rendering would use. `divine-engagement` validates `retryAfter`
/// with zod's `z.iso.datetime()`, which accepts only the UTC designator, and
/// rejects the whole results batch when one field fails.
fn quiet_hours_retry_after(timezone_offset_minutes: i32, now: i64) -> Option<String> {
    let local_seconds = now + i64::from(timezone_offset_minutes) * 60;
    let seconds_today = local_seconds.rem_euclid(86_400);
    let hour = seconds_today / 3_600;
    if (7..21).contains(&hour) {
        return None;
    }

    let until_seven = if hour < 7 {
        7 * 3_600 - seconds_today
    } else {
        86_400 - seconds_today + 7 * 3_600
    };
    chrono::DateTime::from_timestamp(now + until_seven, 0)
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Builds the FCM payload for a campaign notification.
///
/// Data-only, matching the social notification path, so the client keeps
/// control of presentation. `campaignRevisionId` rather than a campaign id,
/// because the revision is what was approved and what the copy belongs to.
fn campaign_payload(delivery: &PendingDelivery, recipient: &PublicKey) -> FcmPayload {
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
        collapse_key: None,
    }
}

/// Delivers one campaign notification, or explains why it will not.
///
/// Order matters. Consent is refused before anything is looked up, so an
/// unconfigured deployment cannot leak the existence of a recipient by
/// behaving differently for one who has devices.
async fn deliver(state: &AppState, delivery: &PendingDelivery, now: i64) -> DeliveryResult {
    let settings = &state.settings.campaign_delivery;

    let refuse = |status, reason: &str| DeliveryResult {
        idempotency_key: delivery.idempotency_key.clone(),
        status,
        reason: Some(reason.to_string()),
        retry_after: None,
    };

    if is_expired(delivery.expires_at.as_deref(), now) {
        return refuse(DeliveryStatus::Suppressed, "campaign_expired");
    }

    let Ok(recipient) = PublicKey::from_str(&delivery.recipient_pubkey) else {
        return refuse(DeliveryStatus::PermanentFailure, "invalid_recipient_pubkey");
    };

    let recipient_hex = recipient.to_hex();
    let allowed = &state.settings.service.allowed_pubkeys;
    if !allowed.is_empty() && !allowed.iter().any(|pubkey| pubkey == &recipient_hex) {
        return refuse(DeliveryStatus::Suppressed, "recipient_not_allowed");
    }

    if !settings.allow_unverified_consent {
        match preferences::campaign_consent_enabled(&state.redis_pool, &recipient_hex).await {
            Ok(true) => {}
            Ok(false) => return refuse(DeliveryStatus::Suppressed, "campaign_consent_disabled"),
            Err(e) => {
                error!(error = %e, key = %delivery.idempotency_key, "Failed to load campaign consent");
                return refuse(DeliveryStatus::RetryableFailure, "consent_lookup_failed");
            }
        }
    }

    // Final idempotency lives here, not in the campaign tool. A lease can
    // expire after we accepted a message but before the result was reported,
    // so the same key will legitimately be offered again.
    //
    // The claim survives only a delivery. It is taken for `CLAIM_TTL_SECS` and
    // promoted to the full dedup window once FCM has accepted a push; every
    // path that took it and ends without one drops it. Holding the full window
    // across a failure would suppress the very retry this function asks for,
    // and the row would settle `already_delivered` for a push that never
    // landed.
    //
    // Both of those writes are scoped to `owner`. Nothing bounds the send, so a
    // claim can expire while this attempt is still inside `send_batch`, and the
    // key may already belong to the re-offer that produced it. Releasing or
    // promoting it then would be operating on someone else's claim.
    let claim = dedup_key(&delivery.idempotency_key);
    let owner = redis_store::new_claim_owner();
    match redis_store::claim_campaign_delivery(&state.redis_pool, &claim, &owner, CLAIM_TTL_SECS)
        .await
    {
        Ok(true) => {}
        Ok(false) => return refuse(DeliveryStatus::Suppressed, "already_delivered"),
        Err(e) => {
            error!(error = %e, key = %delivery.idempotency_key, "Failed to claim campaign delivery");
            return refuse(DeliveryStatus::RetryableFailure, "dedup_unavailable");
        }
    }

    let registered_tokens =
        match redis_store::get_tokens_with_timezone_offsets(&state.redis_pool, &recipient).await {
            Ok(tokens) => tokens,
            Err(e) => {
                error!(error = %e, key = %delivery.idempotency_key, "Failed to load device tokens");
                release_claim(state, &claim, &owner, &delivery.idempotency_key).await;
                return refuse(DeliveryStatus::RetryableFailure, "token_lookup_failed");
            }
        };

    if registered_tokens.is_empty() {
        release_claim(state, &claim, &owner, &delivery.idempotency_key).await;
        return refuse(DeliveryStatus::PermanentFailure, "no_device");
    }

    let mut tokens = Vec::new();
    let mut earliest_retry_after: Option<String> = None;
    let mut timezone_unknown = false;
    for (token, offset) in registered_tokens {
        if settings.allow_unverified_consent {
            tokens.push(token);
            continue;
        }
        let Some(offset) = offset else {
            timezone_unknown = true;
            continue;
        };
        if let Some(retry_after) = quiet_hours_retry_after(offset, now) {
            if earliest_retry_after
                .as_ref()
                .is_none_or(|current| retry_after < *current)
            {
                earliest_retry_after = Some(retry_after);
            }
        } else {
            tokens.push(token);
        }
    }

    if tokens.is_empty() {
        release_claim(state, &claim, &owner, &delivery.idempotency_key).await;
        if let Some(retry_after) = earliest_retry_after {
            return DeliveryResult {
                idempotency_key: delivery.idempotency_key.clone(),
                status: DeliveryStatus::Deferred,
                reason: Some("recipient_quiet_hours".to_string()),
                retry_after: Some(retry_after),
            };
        }
        return refuse(
            DeliveryStatus::Suppressed,
            if timezone_unknown {
                "recipient_timezone_unknown"
            } else {
                "no_device"
            },
        );
    }

    let payload = campaign_payload(delivery, &recipient);
    let outcomes = state.fcm_client.send_batch(&tokens, payload).await;

    let mut delivered_tokens = Vec::new();
    let mut retryable = false;
    for (token, outcome) in outcomes {
        match outcome {
            Ok(()) => delivered_tokens.push(token),
            Err(FcmError::TokenNotRegistered) => {
                // The device is gone. Drop the token so the next campaign does
                // not pay for it again.
                match redis_store::remove_token(&state.redis_pool, &recipient, &token).await {
                    Ok(removed) => record_invalid_token_pruned(removed),
                    Err(e) => {
                        warn!(error = %e, key = %delivery.idempotency_key, "Failed to remove unregistered token")
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, key = %delivery.idempotency_key, "Campaign push failed for one token");
                retryable = true;
            }
        }
    }

    // A delivered campaign push is the same proof of life a delivered social
    // one is, so it has to move the token away from the staleness sweep.
    // `cleanup_stale_tokens` deletes on that score alone and nothing else
    // writes it after registration, so without this a device whose only traffic
    // is campaigns gets deregistered while it is actively receiving them —
    // silently, which is the failure `refresh_token_activity` was added for.
    //
    // Logged rather than propagated, matching the social path: the push has
    // already shipped, and failing here would report a delivered notification
    // as failed and skip the claim promotion below.
    if !delivered_tokens.is_empty() {
        if let Err(e) =
            redis_store::refresh_token_activity(&state.redis_pool, &delivered_tokens).await
        {
            error!(
                error = %e,
                key = %delivery.idempotency_key,
                "Failed to refresh token activity after a delivered campaign push; the sweep may \
                 deregister a live device"
            );
        }
    }

    let accepted = !delivered_tokens.is_empty();
    if accepted {
        // A push landed, so the claim becomes the durable record of it and has
        // to outlive any re-offer.
        // Never below the in-flight window. EXPIRE sets rather than extends,
        // and dedup_ttl_secs is operator-settable with nothing tying it to
        // CLAIM_TTL_SECS, so a shorter value would leave a delivered push
        // holding a shorter claim than an in-flight one. Clamped here rather
        // than with EXPIRE's GT flag, which needs Redis 7 and this repo does
        // not pin the deployed version.
        match redis_store::promote_campaign_delivery(
            &state.redis_pool,
            &claim,
            &owner,
            settings.dedup_ttl_secs.max(CLAIM_TTL_SECS),
        )
        .await
        {
            Ok(true) => {}
            // The send outlived the claim, so a re-offer of this key can push a
            // second time. Both halves of that are the lease budget going
            // unread; the claim window is only standing in for it. Now also
            // covers the claim having been taken over rather than merely
            // expiring — either way it is not ours to extend.
            Ok(false) => {
                warn!(key = %delivery.idempotency_key, "Campaign delivery claim expired or was taken over mid-send")
            }
            Err(e) => {
                warn!(error = %e, key = %delivery.idempotency_key, "Failed to extend campaign delivery claim")
            }
        }
        DeliveryResult {
            idempotency_key: delivery.idempotency_key.clone(),
            status: DeliveryStatus::Delivered,
            reason: None,
            retry_after: None,
        }
    } else if retryable {
        release_claim(state, &claim, &owner, &delivery.idempotency_key).await;
        refuse(DeliveryStatus::RetryableFailure, "provider_error")
    } else {
        release_claim(state, &claim, &owner, &delivery.idempotency_key).await;
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

    let response_body = bounded_response_body(response, MAX_PENDING_RESPONSE_BYTES).await?;
    let pending: PendingResponse = serde_json::from_slice(&response_body).map_err(|e| {
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
    if let Err(e) = validate_api_base_url(&settings.api_base_url) {
        error!(error = %e, "Campaign delivery API URL is unsafe. Not polling.");
        return Ok(());
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
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
    /// skipped run reports green having checked nothing. Refuse to skip wherever
    /// a Redis was asked for: `REDIS_URL` means an operator named one, and `CI`
    /// means the workflow stood one up as a service container.
    ///
    /// `CI` is the half that covers the workflow. It supplies Redis through
    /// `REDIS_HOST`/`REDIS_PORT`, which nothing in this codebase reads — the
    /// coverage exists only because the hardcoded fallback below happens to
    /// match the published port. Keying on `REDIS_URL` alone would leave
    /// exactly the drift this guard exists to catch unguarded.
    async fn test_redis_pool() -> Option<redis_store::RedisPool> {
        let explicit = std::env::var("REDIS_URL").ok();
        let demanded = explicit.is_some() || std::env::var("CI").is_ok();
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

        if reached.is_none() && demanded {
            // Named, not echoed. A Redis URL carries `[:password@]`, and this
            // is a test panic, so it lands in whatever collects the run's
            // output. `state.rs` logs the source of this variable and never its
            // value, for the same reason.
            panic!(
                "no Redis at {} and one was demanded (REDIS_URL or CI set); refusing to skip",
                if explicit.is_some() {
                    "REDIS_URL"
                } else {
                    "the default localhost:6379"
                }
            );
        }
        reached
    }

    /// `state_with_sender` for the common `MockFcmSender` case.
    fn sending_state(pool: redis_store::RedisPool, sender: MockFcmSender) -> AppState {
        state_with_sender(pool, Box::new(sender))
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

    /// The value a claim key is holding, or `None` when it is gone.
    async fn claim_value(pool: &redis_store::RedisPool, key: &str) -> Option<String> {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("GET")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .unwrap()
    }

    /// Deletes a claim key outright.
    ///
    /// Tests cannot go through `release_campaign_delivery`: the owner token is
    /// minted inside `deliver` and never leaves it, so a compare-and-delete
    /// from out here would be a silent no-op.
    async fn delete_claim(pool: &redis_store::RedisPool, key: &str) {
        let mut conn = pool.get().await.unwrap();
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .unwrap();
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

    /// An FCM stub that drops the claim mid-send.
    ///
    /// Reaches the branch where the send outlived the claim, which the code
    /// itself flags as producing a second push on the next re-offer.
    struct ClaimDestroyer {
        pool: redis_store::RedisPool,
        claim: String,
    }

    #[async_trait::async_trait]
    impl crate::fcm_sender::FcmSend for ClaimDestroyer {
        async fn send_single(
            &self,
            _token: &str,
            _payload: FcmPayload,
        ) -> std::result::Result<(), FcmError> {
            // A raw delete: this stands in for the claim expiring, and the
            // stub does not own it, so the compare-and-delete release would be
            // a no-op here.
            delete_claim(&self.pool, &self.claim).await;
            Ok(())
        }
    }

    /// An FCM stub that hands this attempt's claim to a successor mid-send.
    ///
    /// Exactly what a re-offer does behind a slow send: the claim lapses, the
    /// row is leased again, and the second attempt claims the same key for
    /// itself. Whatever the first attempt does next must not touch it.
    struct ClaimUsurper {
        pool: redis_store::RedisPool,
        claim: String,
        outcome: std::result::Result<(), FcmError>,
    }

    #[async_trait::async_trait]
    impl crate::fcm_sender::FcmSend for ClaimUsurper {
        async fn send_single(
            &self,
            _token: &str,
            _payload: FcmPayload,
        ) -> std::result::Result<(), FcmError> {
            delete_claim(&self.pool, &self.claim).await;
            let mut conn = self.pool.get().await.unwrap();
            let taken: Option<String> = redis::cmd("SET")
                .arg(&self.claim)
                .arg(SUCCESSOR_OWNER)
                .arg("NX")
                .arg("EX")
                .arg(CLAIM_TTL_SECS)
                .query_async(&mut *conn)
                .await
                .unwrap();
            assert!(taken.is_some(), "the successor must get the claim");
            self.outcome.clone()
        }
    }

    /// The owner token `ClaimUsurper` plants, standing in for a second attempt.
    const SUCCESSOR_OWNER: &str = "successor-attempt";

    /// State whose consent gate is open, built around any `FcmSend` stub.
    ///
    /// The one place the whole `AppState` is spelled out, so a new field is one
    /// edit rather than one per test.
    fn state_with_sender(
        pool: redis_store::RedisPool,
        sender: Box<dyn crate::fcm_sender::FcmSend>,
    ) -> AppState {
        let mut settings = crate::config::Settings::new().unwrap();
        settings.campaign_delivery.allow_unverified_consent = true;
        AppState {
            settings,
            redis_pool: pool,
            fcm_client: Arc::new(FcmClient::new_with_impl(sender)),
            service_keys: None,
            crypto_service: None,
            nostr_client: Arc::new(Client::default()),
            profile_client: Arc::new(Client::default()),
            mention_parser_service: None,
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
    fn test_minute_precision_expiry_matches_engagement_schema() {
        let expiry = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:30:00Z")
            .unwrap()
            .timestamp();
        assert!(is_expired(Some("2026-01-01T12:30Z"), expiry));
        assert!(!is_expired(Some("2026-01-01T12:30Z"), expiry - 1));
    }

    #[test]
    fn test_campaign_api_requires_a_plain_https_origin() {
        assert!(validate_api_base_url("https://engagement.admin.divine.video").is_ok());
        for unsafe_url in [
            "http://engagement.admin.divine.video",
            "https://user:secret@engagement.admin.divine.video",
            "https://engagement.admin.divine.video/path",
            "https://engagement.admin.divine.video?redirect=elsewhere",
        ] {
            assert!(
                validate_api_base_url(unsafe_url).is_err(),
                "{unsafe_url} must be rejected"
            );
        }
    }

    #[test]
    fn test_quiet_hours_use_recipient_local_offset() {
        let noon_utc = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(quiet_hours_retry_after(0, noon_utc), None);

        // `Z`, not `+00:00`: divine-engagement parses retryAfter with zod's
        // `z.iso.datetime()`, which rejects an offset-form UTC timestamp and
        // then rejects the entire results batch.
        let retry = quiet_hours_retry_after(600, noon_utc).expect("22:00 local is quiet");
        assert_eq!(retry, "2026-01-01T21:00:00Z");

        let retry = quiet_hours_retry_after(-480, noon_utc).expect("04:00 local is quiet");
        assert_eq!(retry, "2026-01-01T15:00:00Z");
    }

    #[test]
    fn test_quiet_hours_boundaries_are_fail_closed() {
        let at_seven = chrono::DateTime::parse_from_rfc3339("2026-01-01T07:00:00Z")
            .unwrap()
            .timestamp();
        let at_twenty_one = chrono::DateTime::parse_from_rfc3339("2026-01-01T21:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(quiet_hours_retry_after(0, at_seven), None);
        assert!(quiet_hours_retry_after(0, at_twenty_one).is_some());
    }

    #[test]
    fn test_payload_is_data_only_and_carries_every_delivery_value() {
        // Whole-map equality, not spot checks, and not a search of the encoded
        // form for the substring "token" — real FCM registration tokens do not
        // contain that word, so a substring check cannot detect one. `HashMap`
        // equality is length plus per-key value equality, so this pins the key
        // set and which delivery field feeds each key at once: an extra key, a
        // missing one, and a wrong-source insert (the tap type where the tap
        // value goes, title and body swapped) all fail here.
        let recipient = Keys::generate().public_key();
        let payload = campaign_payload(&delivery(None), &recipient);

        assert!(payload.notification.is_none(), "must stay data-only");
        assert!(
            payload.collapse_key.is_none(),
            "campaigns are not like/repost coalescing groups"
        );
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

        assert_eq!(
            data, expected,
            "campaign payload changed; confirm no device token or other \
             sensitive field was introduced before updating this map"
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
            (DeliveryStatus::Deferred, "deferred"),
            (DeliveryStatus::Suppressed, "suppressed"),
            (DeliveryStatus::PermanentFailure, "permanent_failure"),
            (DeliveryStatus::RetryableFailure, "retryable_failure"),
        ] {
            let encoded = serde_json::to_string(&DeliveryResult {
                idempotency_key: "k".to_string(),
                status,
                reason: None,
                retry_after: None,
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
                retry_after: None,
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

        delete_claim(&pool, &claim).await;
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

        delete_claim(&pool, &claim).await;
    }

    #[tokio::test]
    async fn test_a_send_that_outlives_its_claim_still_reports_delivered() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "outlived").await;
        let claim = dedup_key(&pending.idempotency_key);

        let destroyer = ClaimDestroyer {
            pool: pool.clone(),
            claim: claim.clone(),
        };
        let mut state = sending_state(pool.clone(), MockFcmSender::new());
        state.fcm_client = Arc::new(FcmClient::new_with_impl(Box::new(destroyer)));

        // A push landed, so the outcome is Delivered whatever the claim did.
        let result = deliver(&state, &pending, 1_800_000_000).await;
        assert_eq!(result.status, DeliveryStatus::Delivered);
        assert_eq!(result.reason, None);

        // Nothing was left to promote, so the key does not come back: a
        // re-offer of this row would push a second time. That is the lease
        // budget going unread, and this pins the behaviour rather than
        // endorsing it.
        assert_eq!(
            claim_ttl(&pool, &claim).await,
            -2,
            "a claim destroyed mid-send must not be resurrected by the promote"
        );
    }

    #[tokio::test]
    async fn test_a_failed_send_does_not_release_a_successors_claim() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "usurp-release").await;
        let claim = dedup_key(&pending.idempotency_key);

        // The claim lapses mid-send and the re-offer it produces claims the key
        // for itself. This attempt then fails and releases.
        let state = state_with_sender(
            pool.clone(),
            Box::new(ClaimUsurper {
                pool: pool.clone(),
                claim: claim.clone(),
                outcome: Err(FcmError::InternalError),
            }),
        );
        let result = deliver(&state, &pending, 1_800_000_000).await;
        assert_eq!(result.status, DeliveryStatus::RetryableFailure);

        // An unguarded DEL here deletes the successor's live claim, after which
        // a third attempt claims and pushes to someone the successor is already
        // pushing to.
        assert_eq!(
            claim_value(&pool, &claim).await.as_deref(),
            Some(SUCCESSOR_OWNER),
            "a stale release must not delete a claim this attempt no longer owns"
        );

        delete_claim(&pool, &claim).await;
    }

    #[tokio::test]
    async fn test_a_late_send_does_not_promote_a_successors_claim() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "usurp-promote").await;
        let claim = dedup_key(&pending.idempotency_key);

        // Same handover, but this attempt's send succeeds, so it promotes.
        let state = state_with_sender(
            pool.clone(),
            Box::new(ClaimUsurper {
                pool: pool.clone(),
                claim: claim.clone(),
                outcome: Ok(()),
            }),
        );
        let result = deliver(&state, &pending, 1_800_000_000).await;
        assert_eq!(result.status, DeliveryStatus::Delivered);

        // An unguarded EXPIRE here gives the successor's *in-flight* claim the
        // seven-day dedup window, so a push that has not landed yet is recorded
        // as delivered and the successor's own retry is suppressed.
        assert_eq!(
            claim_value(&pool, &claim).await.as_deref(),
            Some(SUCCESSOR_OWNER),
            "the successor's claim must survive untouched"
        );
        let ttl = claim_ttl(&pool, &claim).await;
        assert!(
            ttl <= CLAIM_TTL_SECS as i64,
            "a stale promote must not extend a claim this attempt no longer owns; TTL was {ttl}"
        );

        delete_claim(&pool, &claim).await;
    }

    #[tokio::test]
    async fn test_promotion_never_shortens_the_in_flight_claim() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "shortdedup").await;
        let claim = dedup_key(&pending.idempotency_key);

        // An operator setting dedup_ttl_secs below CLAIM_TTL_SECS must not end
        // up with a delivered push held for less time than an in-flight one.
        let mut state = sending_state(pool.clone(), MockFcmSender::new());
        state.settings.campaign_delivery.dedup_ttl_secs = 60;

        let result = deliver(&state, &pending, 1_800_000_000).await;
        assert_eq!(result.status, DeliveryStatus::Delivered);

        // Same few seconds of slack the sibling assertion allows for the round
        // trip: Redis rounds TTL to the nearest second, so a bare `>=` tolerates
        // about half a second between the promote and this read.
        let ttl = claim_ttl(&pool, &claim).await;
        assert!(
            ttl >= CLAIM_TTL_SECS as i64 - 5,
            "a delivered claim must never be shorter than the in-flight window; TTL was {ttl}"
        );

        delete_claim(&pool, &claim).await;
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
        let state = state_with_sender(pool.clone(), Box::new(probe));
        let dedup_ttl = state.settings.campaign_delivery.dedup_ttl_secs as i64;

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

        delete_claim(&pool, &claim).await;
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

        delete_claim(&pool, &claim).await;
    }

    #[test]
    fn test_a_pruned_campaign_token_is_counted_once_and_only_when_removed() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            record_invalid_token_pruned(true);
            record_invalid_token_pruned(false);
        });

        handle.run_upkeep();
        let rendered = handle.render();
        // The same series the social path writes. A campaign prune that landed
        // on its own label, or on none, would leave the sweep's own accounting
        // reading low with nothing to say so.
        assert!(
            rendered.contains(r#"push_tokens_pruned_total{reason="invalid"} 1"#),
            "{rendered}"
        );
    }

    /// The `stale_tokens` score for a token, or `None` when it is not tracked.
    async fn stale_score(pool: &redis_store::RedisPool, token: &str) -> Option<f64> {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZSCORE")
            .arg("stale_tokens")
            .arg(token)
            .query_async(&mut *conn)
            .await
            .unwrap()
    }

    /// Backdates a token's staleness score so a refresh has something to move.
    ///
    /// `XX` without `GT`, so this can move the score down where
    /// `refresh_token_activity` deliberately cannot.
    async fn backdate_stale_score(pool: &redis_store::RedisPool, token: &str, score: i64) {
        let mut conn = pool.get().await.unwrap();
        let _: i64 = redis::cmd("ZADD")
            .arg("stale_tokens")
            .arg("XX")
            .arg("CH")
            .arg(score)
            .arg(token)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_delivered_campaign_push_keeps_the_device_off_the_sweep() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, token) = registered_delivery(&pool, "activity").await;
        let claim = dedup_key(&pending.idempotency_key);

        // Registration wrote today's score. Age it past any plausible sweep
        // window so that a refresh is the only thing that can bring it back.
        backdate_stale_score(&pool, &token, 1_000).await;
        assert_eq!(stale_score(&pool, &token).await, Some(1_000.0));

        let result = deliver(
            &sending_state(pool.clone(), MockFcmSender::new()),
            &pending,
            1_800_000_000,
        )
        .await;
        assert_eq!(result.status, DeliveryStatus::Delivered);

        // Without the refresh the score stays where it was backdated to, and
        // cleanup_stale_tokens deregisters a device that just took a push.
        let score = stale_score(&pool, &token)
            .await
            .expect("a delivered token must stay tracked");
        assert!(
            score > 1_000.0,
            "a delivered campaign push must move its token off the staleness \
             sweep; score was still {score}"
        );

        delete_claim(&pool, &claim).await;
    }

    /// Consent precedes every lookup, so a user who never opted in is
    /// suppressed without a token read or an FCM send, and only an explicit
    /// `campaignsEnabled` write flips it.
    #[tokio::test]
    async fn test_campaign_delivery_requires_an_explicit_consent_opt_in() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, token) = registered_delivery(&pool, "consent").await;
        let recipient = PublicKey::from_str(&pending.recipient_pubkey).unwrap();
        redis_store::add_or_update_token_with_timezone(&pool, &recipient, &token, Some(0))
            .await
            .unwrap();

        let noon = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .timestamp();
        let mut state = sending_state(pool.clone(), MockFcmSender::new());
        state.settings.campaign_delivery.allow_unverified_consent = false;

        // No consent key at all reads as false, not as an unknown to retry.
        let without_consent = deliver(&state, &pending, noon).await;
        assert_eq!(without_consent.status, DeliveryStatus::Suppressed);
        assert_eq!(
            without_consent.reason.as_deref(),
            Some("campaign_consent_disabled")
        );

        preferences::set_user_preferences_with_campaign_consent(
            &pool,
            &pending.recipient_pubkey,
            &preferences::UserPreferences { kinds: vec![7] },
            true,
        )
        .await
        .unwrap();

        let working = MockFcmSender::new();
        let mut state = sending_state(pool.clone(), working.clone());
        state.settings.campaign_delivery.allow_unverified_consent = false;
        let with_consent = deliver(&state, &pending, noon).await;
        assert_eq!(with_consent.status, DeliveryStatus::Delivered);
        assert_eq!(working.get_sent_messages().len(), 1);
    }

    /// A deferral is not a delivery: it has to come back with the instant the
    /// retry becomes allowed, in the only format the engagement API parses,
    /// and it must free the claim so that retry can actually send.
    #[tokio::test]
    async fn test_quiet_hours_defer_carries_a_z_suffixed_retry_after() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, token) = registered_delivery(&pool, "quiet").await;
        let recipient = PublicKey::from_str(&pending.recipient_pubkey).unwrap();
        redis_store::add_or_update_token_with_timezone(&pool, &recipient, &token, Some(600))
            .await
            .unwrap();
        preferences::set_user_preferences_with_campaign_consent(
            &pool,
            &pending.recipient_pubkey,
            &preferences::UserPreferences { kinds: vec![7] },
            true,
        )
        .await
        .unwrap();

        // UTC+10 at 12:00Z is 22:00 local, so the next allowed instant is
        // 07:00 local = 21:00Z.
        let noon = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .timestamp();
        let mut state = sending_state(pool.clone(), MockFcmSender::new());
        state.settings.campaign_delivery.allow_unverified_consent = false;

        let result = deliver(&state, &pending, noon).await;
        assert_eq!(result.status, DeliveryStatus::Deferred);
        assert_eq!(result.reason.as_deref(), Some("recipient_quiet_hours"));
        assert_eq!(result.retry_after.as_deref(), Some("2026-01-01T21:00:00Z"));
        assert_eq!(
            claim_ttl(&pool, &dedup_key(&pending.idempotency_key)).await,
            -2,
            "a deferral must release its claim so the scheduled retry can send"
        );
    }

    /// A registration without a timezone is still a device registration; it is
    /// only ineligible for campaigns, and it must not be reported as a missing
    /// device to the campaign tool.
    #[tokio::test]
    async fn test_a_registration_without_a_timezone_is_ineligible_for_campaigns() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let (pending, _token) = registered_delivery(&pool, "no-tz").await;
        preferences::set_user_preferences_with_campaign_consent(
            &pool,
            &pending.recipient_pubkey,
            &preferences::UserPreferences { kinds: vec![7] },
            true,
        )
        .await
        .unwrap();

        let noon = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .timestamp();
        let mut state = sending_state(pool.clone(), MockFcmSender::new());
        state.settings.campaign_delivery.allow_unverified_consent = false;

        let result = deliver(&state, &pending, noon).await;
        assert_eq!(result.status, DeliveryStatus::Suppressed);
        assert_eq!(result.reason.as_deref(), Some("recipient_timezone_unknown"));
        assert_eq!(result.retry_after, None);
    }
}
