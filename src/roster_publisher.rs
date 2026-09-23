//! Uploads the campaign opt-in roster to divine-engagement.
//!
//! Consent and device registration live here; the campaign tool needs to know
//! who may be campaigned to without ever seeing a token. The direction matches
//! campaign delivery: GKE reaches out to Cloudflare, never the reverse.

use crate::{campaign_delivery, error::Result, preferences, redis_store, state::AppState};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Reduce (pubkey, campaign consent, device token count) triples to the roster.
///
/// A person with consent but no device is left out: uploading them would
/// inflate the audience estimate with recipients who can only ever settle as
/// `no_device`.
pub(crate) fn build_roster(entries: Vec<(String, bool, usize)>) -> Vec<String> {
    let mut roster: Vec<String> = entries
        .into_iter()
        .filter(|(_, consented, device_count)| *consented && *device_count > 0)
        .map(|(pubkey, _, _)| pubkey)
        .collect();
    roster.sort();
    roster.dedup();
    roster
}

/// Collect every registered pubkey that has campaign consent and a device.
pub async fn collect_opted_in(state: &AppState) -> Result<Vec<String>> {
    let mut entries = Vec::new();
    for pubkey in redis_store::all_registered_pubkeys(&state.redis_pool).await? {
        let consented = preferences::campaign_consent_enabled(&state.redis_pool, &pubkey).await?;
        let device_count = match nostr_sdk::PublicKey::from_hex(&pubkey) {
            Ok(parsed) => redis_store::get_tokens_for_pubkey(&state.redis_pool, &parsed)
                .await?
                .len(),
            Err(_) => 0,
        };
        entries.push((pubkey, consented, device_count));
    }
    Ok(build_roster(entries))
}

/// Publish the opt-in roster to divine-engagement on a fixed interval.
///
/// Returns immediately when the interval is zero or the API base URL is unset,
/// so an unconfigured deployment is closed rather than failing on a timer.
pub async fn run_roster_publisher(state: Arc<AppState>, token: CancellationToken) -> Result<()> {
    let settings = state.settings.campaign_delivery.clone();

    if settings.roster_publish_interval_secs == 0 || settings.api_base_url.is_empty() {
        info!("Opt-in roster publishing is disabled.");
        return Ok(());
    }
    if let Err(e) = campaign_delivery::validate_api_base_url(&settings.api_base_url) {
        error!(error = %e, "Opt-in roster API URL is unsafe. Not publishing.");
        return Ok(());
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| crate::error::ServiceError::Internal(format!("HTTP client: {e}")))?;

    let mut ticker = interval(Duration::from_secs(
        settings.roster_publish_interval_secs.max(1),
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    info!(
        interval_secs = settings.roster_publish_interval_secs,
        "Starting opt-in roster publishing."
    );

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Opt-in roster publishing cancelled. Shutting down...");
                break;
            }
            _ = ticker.tick() => {
                match publish_once(&state, &http).await {
                    // An empty roster is refused upstream, so uploading it would
                    // only turn a publisher bug into a failed request. The log
                    // is the signal; the previous roster stays in place.
                    Ok(0) => warn!("Opt-in roster is empty; skipped the upload."),
                    Ok(count) => info!(count, "Published the opt-in roster."),
                    Err(e) => error!(error = %e, "Opt-in roster publish failed."),
                }
            }
        }
    }

    Ok(())
}

/// Collect and upload one roster snapshot. Returns how many pubkeys were sent.
async fn publish_once(state: &AppState, http: &reqwest::Client) -> Result<usize> {
    let settings = &state.settings.campaign_delivery;
    let roster = collect_opted_in(state).await?;
    if roster.is_empty() {
        return Ok(0);
    }

    let url = format!(
        "{}/api/internal/audience/opted-in",
        settings.api_base_url.trim_end_matches('/')
    );
    let response = campaign_delivery::with_access_headers(http.post(&url), settings)
        .json(&serde_json::json!({ "pubkeys": roster }))
        .send()
        .await
        .map_err(|e| {
            crate::error::ServiceError::Internal(format!("Opt-in roster upload failed: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(crate::error::ServiceError::Internal(format!(
            "Opt-in roster upload returned {status}: {}",
            body.chars().take(2048).collect::<String>()
        )));
    }

    Ok(roster.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_excludes_unregistered_and_unconsented_pubkeys() {
        let consented_with_device = "a".repeat(64);
        let consented_no_device = "b".repeat(64);
        let registered_no_consent = "c".repeat(64);

        let roster = build_roster(vec![
            (consented_with_device.clone(), true, 2),
            (consented_no_device.clone(), true, 0),
            (registered_no_consent.clone(), false, 3),
        ]);

        assert_eq!(roster, vec![consented_with_device]);
    }

    #[test]
    fn roster_is_sorted_and_deduplicated() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let roster = build_roster(vec![
            (b.clone(), true, 1),
            (a.clone(), true, 1),
            (b.clone(), true, 1),
        ]);
        assert_eq!(roster, vec![a, b]);
    }
}
