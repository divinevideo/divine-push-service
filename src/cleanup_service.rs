use crate::{error::Result, redis_store, state::AppState};
use std::{sync::Arc, time::Duration};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Runs the periodic cleanup task for stale FCM tokens.
pub async fn run_cleanup_service(state: Arc<AppState>, token: CancellationToken) -> Result<()> {
    if !state.settings.cleanup.enabled {
        warn!("Token cleanup service is disabled in settings.");
        return Ok(());
    }

    let interval_seconds = state.settings.cleanup.interval_secs;
    let max_age_days = state.settings.cleanup.token_max_age_days;

    if interval_seconds == 0 || max_age_days <= 0 {
        error!(
            interval = interval_seconds,
            max_age = max_age_days,
            "Invalid cleanup settings. Disabling cleanup service."
        );
        return Ok(());
    }

    let max_age_seconds = max_age_days * 24 * 60 * 60;
    let mut interval_timer = interval(Duration::from_secs(interval_seconds));

    info!(
        interval_secs = interval_seconds,
        max_age_days = max_age_days,
        "Starting token cleanup service."
    );

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cleanup service cancellation received. Shutting down...");
                break;
            }
            _ = interval_timer.tick() => {
                info!("Running stale token cleanup...");

                tokio::select! {
                     biased;
                     _ = token.cancelled() => {
                         info!("Cleanup service cancelled during Redis cleanup operation.");
                         break;
                     }
                     cleanup_result = redis_store::cleanup_stale_tokens(&state.redis_pool, max_age_seconds) => {
                           match cleanup_result {
                                Ok(cleaned_count) => {
                                    if cleaned_count > 0 {
                                        record_stale_tokens_pruned(cleaned_count);
                                        info!(count = cleaned_count, "Cleaned up stale tokens.");
                                    } else {
                                        info!("No stale tokens found to cleanup.");
                                    }
                                }
                                Err(e) => {
                                    error!(error = %e, "Error during stale token cleanup");
                                }
                            }
                     }
                }
            }
        }
    }
    info!("Cleanup service shut down.");
    Ok(())
}

fn record_stale_tokens_pruned(count: usize) {
    crate::metrics::tokens_pruned("stale", count as u64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    #[test]
    fn stale_cleanup_records_the_confirmed_redis_count() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || record_stale_tokens_pruned(3));

        handle.run_upkeep();
        let rendered = handle.render();
        assert!(
            rendered.contains(r#"push_tokens_pruned_total{reason="stale"} 3"#),
            "{rendered}"
        );
    }
}
