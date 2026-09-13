//! Prometheus metrics for relay ingestion and push delivery.

use chrono::Utc;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

pub const EVENTS_RECEIVED_TOTAL: &str = "push_events_received_total";
pub const EVENTS_PROCESSED_TOTAL: &str = "push_events_processed_total";
pub const FCM_SENDS_ATTEMPTED_TOTAL: &str = "push_fcm_sends_attempted_total";
pub const FCM_SENDS_SUCCEEDED_TOTAL: &str = "push_fcm_sends_succeeded_total";
pub const FCM_SENDS_FAILED_TOTAL: &str = "push_fcm_sends_failed_total";
pub const NEW_POST_FANOUT_RETRIES_TOTAL: &str = "push_new_post_fanout_retries_total";
pub const INTERNAL_DM_REQUESTS_TOTAL: &str = "push_internal_dm_requests_total";
pub const TOKENS_PRUNED_TOTAL: &str = "push_tokens_pruned_total";
pub const COALESCED_SENDS_TOTAL: &str = "push_coalesced_sends_total";
pub const COALESCE_OLDEST_DUE_AGE_SECONDS: &str = "push_coalesce_oldest_due_age_seconds";
pub const THROTTLED_RECIPIENTS_TOTAL: &str = "push_throttled_recipients_total";
pub const LAST_EVENT_PROCESSED_TIMESTAMP_SECONDS: &str =
    "push_last_event_processed_timestamp_seconds";

/// Installs the process-wide Prometheus recorder and initializes its metrics.
pub fn init() -> PrometheusHandle {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    metrics::describe_counter!(
        EVENTS_RECEIVED_TOTAL,
        "Nostr events received from the relay"
    );
    metrics::describe_counter!(
        EVENTS_PROCESSED_TOTAL,
        "Nostr events whose handler routing attempt completed"
    );
    metrics::describe_counter!(
        FCM_SENDS_ATTEMPTED_TOTAL,
        "FCM delivery attempts, counted per device token"
    );
    metrics::describe_counter!(
        FCM_SENDS_SUCCEEDED_TOTAL,
        "Successful FCM deliveries, counted per device token"
    );
    metrics::describe_counter!(
        FCM_SENDS_FAILED_TOTAL,
        "Failed FCM deliveries, counted per device token and reason"
    );
    metrics::describe_counter!(
        NEW_POST_FANOUT_RETRIES_TOTAL,
        "Durable new-post fan-out retries and terminal expiry, counted by bounded reason and outcome"
    );
    metrics::describe_counter!(
        INTERNAL_DM_REQUESTS_TOTAL,
        "Internal direct-message requests, counted by bounded outcome"
    );
    metrics::describe_counter!(
        TOKENS_PRUNED_TOTAL,
        "FCM tokens removed by the service, counted by reason"
    );
    metrics::describe_counter!(
        COALESCED_SENDS_TOTAL,
        "Coalesced like/repost summary pushes sent, counted by notification type"
    );
    metrics::describe_gauge!(
        COALESCE_OLDEST_DUE_AGE_SECONDS,
        "Age of the claimed coalescing group's bucket deadline at claim time"
    );
    metrics::describe_counter!(
        THROTTLED_RECIPIENTS_TOTAL,
        "Would-be immediate like/repost pushes demoted into the coalescing bucket by the per-recipient throttle"
    );
    metrics::describe_gauge!(
        LAST_EVENT_PROCESSED_TIMESTAMP_SECONDS,
        "Unix timestamp of the last completed event routing attempt"
    );

    // Give a newly started instance one deadman window to receive an event.
    set_last_event_processed_timestamp(Utc::now().timestamp() as f64);
    handle
}

pub fn event_received() {
    metrics::counter!(EVENTS_RECEIVED_TOTAL).increment(1);
}

pub fn event_processed() {
    metrics::counter!(EVENTS_PROCESSED_TOTAL).increment(1);
    set_last_event_processed_timestamp(Utc::now().timestamp() as f64);
}

pub fn fcm_send_attempted() {
    metrics::counter!(FCM_SENDS_ATTEMPTED_TOTAL).increment(1);
}

pub fn fcm_send_succeeded() {
    metrics::counter!(FCM_SENDS_SUCCEEDED_TOTAL).increment(1);
}

pub fn fcm_send_failed(reason: &'static str) {
    metrics::counter!(FCM_SENDS_FAILED_TOTAL, "reason" => reason).increment(1);
}

pub fn new_post_fanout_retry(reason: &'static str, outcome: &'static str) {
    metrics::counter!(
        NEW_POST_FANOUT_RETRIES_TOTAL,
        "reason" => reason,
        "outcome" => outcome
    )
    .increment(1);
}

pub fn internal_dm_request(outcome: &'static str) {
    metrics::counter!(INTERNAL_DM_REQUESTS_TOTAL, "outcome" => outcome).increment(1);
}

pub fn tokens_pruned(reason: &'static str, count: u64) {
    metrics::counter!(TOKENS_PRUNED_TOTAL, "reason" => reason).increment(count);
}

pub fn coalesced_send(notification_type: &'static str) {
    metrics::counter!(COALESCED_SENDS_TOTAL, "type" => notification_type).increment(1);
}

pub fn coalesce_oldest_due_age(age_seconds: f64) {
    metrics::gauge!(COALESCE_OLDEST_DUE_AGE_SECONDS).set(age_seconds);
}

pub fn throttled_recipient(notification_type: &'static str) {
    metrics::counter!(THROTTLED_RECIPIENTS_TOTAL, "type" => notification_type).increment(1);
}

fn set_last_event_processed_timestamp(timestamp: f64) {
    metrics::gauge!(LAST_EVENT_PROCESSED_TIMESTAMP_SECONDS).set(timestamp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_metrics_render_with_bounded_labels() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            event_received();
            event_processed();
            fcm_send_attempted();
            fcm_send_succeeded();
            fcm_send_failed("unauthorized");
            new_post_fanout_retry("delivery", "scheduled");
            internal_dm_request("unauthorized");
            tokens_pruned("invalid", 1);
            coalesced_send("like");
            coalesce_oldest_due_age(12.0);
            throttled_recipient("repost");
        });

        handle.run_upkeep();
        let rendered = handle.render();
        assert!(
            rendered.contains("push_events_received_total 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("push_events_processed_total 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("push_fcm_sends_attempted_total 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("push_fcm_sends_succeeded_total 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"push_fcm_sends_failed_total{reason="unauthorized"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                r#"push_new_post_fanout_retries_total{reason="delivery",outcome="scheduled"} 1"#
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"push_internal_dm_requests_total{outcome="unauthorized"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"push_tokens_pruned_total{reason="invalid"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"push_coalesced_sends_total{type="like"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains("push_coalesce_oldest_due_age_seconds 12"),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"push_throttled_recipients_total{type="repost"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains("push_last_event_processed_timestamp_seconds"),
            "{rendered}"
        );
    }
}
