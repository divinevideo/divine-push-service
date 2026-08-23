//! HTTP server exposing health and Prometheus metrics.

use axum::{
    extract::State,
    http::{header, StatusCode},
    routing::get,
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::health::{CriticalTask, TaskHealth};
use crate::state::AppState;

/// Router state: the health endpoint reports both service identity and the
/// liveness of the delivery tasks.
#[derive(Clone)]
pub struct ServerState {
    service_pubkey: Option<String>,
    health: Arc<TaskHealth>,
    metrics: PrometheusHandle,
}

impl ServerState {
    pub fn new(
        service_pubkey: Option<String>,
        health: Arc<TaskHealth>,
        metrics: PrometheusHandle,
    ) -> Self {
        Self {
            service_pubkey,
            health,
            metrics,
        }
    }
}

/// Reports `503` when any critical task has died, so a pod whose delivery
/// pipeline is dead fails its probes instead of serving `200` forever.
///
/// Both the liveness and readiness probes point here, so a dead pipeline is
/// restarted rather than silently retained behind a healthy-looking pod.
async fn health_check(State(state): State<ServerState>) -> (StatusCode, Json<serde_json::Value>) {
    let tasks: serde_json::Map<String, serde_json::Value> = CriticalTask::ALL
        .iter()
        .map(|task| {
            (
                task.name().to_string(),
                serde_json::Value::Bool(state.health.is_alive(*task)),
            )
        })
        .collect();

    let healthy = state.health.all_alive();

    (
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(serde_json::json!({
            "status": if healthy { "ok" } else { "degraded" },
            "pubkey": state.service_pubkey,
            "tasks": tasks,
        })),
    )
}

async fn metrics(
    State(state): State<ServerState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CACHE_CONTROL, "no-store")],
        state.metrics.render(),
    )
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics))
        .with_state(state)
}

pub async fn run_server(
    app_state: Arc<AppState>,
    health: Arc<TaskHealth>,
    metrics: PrometheusHandle,
    token: CancellationToken,
) {
    let app = router(ServerState::new(
        app_state.service_pubkey_hex(),
        health,
        metrics,
    ));

    // Failure paths below deliberately just return. Cancelling the token here
    // would make this look like an ordinary shutdown to the supervising
    // `TaskExitGuard`, which would then skip flagging an unexpected exit and let
    // the process exit 0 on what is really a startup failure. Returning lets the
    // guard classify it and cancel on our behalf.
    let listen_addr_str = &app_state.settings.server.listen_addr;
    let addr: SocketAddr = match listen_addr_str.parse() {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!(
                "Invalid server.listen_addr '{}': {}. Exiting server task.",
                listen_addr_str,
                e
            );
            return;
        }
    };

    tracing::info!("HTTP server listening on {}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!("Failed to bind HTTP server: {}", e);
            return;
        }
    };

    let shutdown_token = token.clone();
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("HTTP server shutting down.");
        })
        .await
    {
        tracing::error!("HTTP server error: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tower::ServiceExt;

    fn server_state(health: Arc<TaskHealth>) -> ServerState {
        let metrics = PrometheusBuilder::new().build_recorder().handle();
        ServerState::new(Some("pubkey-hex".to_string()), health, metrics)
    }

    async fn get_health(health: Arc<TaskHealth>) -> (StatusCode, serde_json::Value) {
        let response = router(server_state(health))
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn healthy_service_reports_ok() {
        let (status, body) = get_health(Arc::new(TaskHealth::new())).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["pubkey"], "pubkey-hex");
        assert_eq!(body["tasks"]["event_handler"], true);
        assert_eq!(body["tasks"]["nostr_listener"], true);
    }

    /// The probe behaviour the outage needed: a dead event handler must fail
    /// the probe so Kubernetes restarts the pod.
    #[tokio::test]
    async fn dead_event_handler_fails_the_probe() {
        let health = Arc::new(TaskHealth::new());
        health.mark_dead(CriticalTask::EventHandler);

        let (status, body) = get_health(health).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["tasks"]["event_handler"], false);
        // The surviving task is still reported, so the log identifies which
        // half of the pipeline died.
        assert_eq!(body["tasks"]["nostr_listener"], true);
    }

    /// `run_server` must not cancel the token on its own failure paths. If it
    /// did, its supervising guard would read the cancelled token as an ordinary
    /// shutdown, skip flagging an unexpected exit, and let a failed bind exit 0.
    #[test]
    fn run_server_does_not_self_cancel_on_failure() {
        let source = include_str!("server.rs");
        let body = source
            .split("pub async fn run_server")
            .nth(1)
            .expect("run_server must exist")
            .split("#[cfg(test)]")
            .next()
            .expect("body before tests");

        assert!(
            !body.contains("token.cancel()"),
            "run_server must leave cancellation to its TaskExitGuard so the \
             failure is classified and the process exits non-zero"
        );
    }

    #[tokio::test]
    async fn dead_nostr_listener_fails_the_probe() {
        let health = Arc::new(TaskHealth::new());
        health.mark_dead(CriticalTask::NostrListener);

        let (status, _body) = get_health(health).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn metrics_are_rendered_without_caching() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        ::metrics::with_local_recorder(&recorder, crate::metrics::event_received);
        let response = router(ServerState::new(
            Some("pubkey-hex".to_string()),
            Arc::new(TaskHealth::new()),
            handle,
        ))
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains("push_events_received_total 1"));
    }
}
