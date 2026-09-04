//! HTTP server exposing health and Prometheus metrics.

use async_trait::async_trait;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use nostr_sdk::{EventId, PublicKey};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::event_handler;
use crate::health::{CriticalTask, TaskHealth};
use crate::state::AppState;

#[async_trait]
trait DirectMessageDelivery: Send + Sync {
    async fn deliver(&self, event_id: EventId, recipient: PublicKey) -> Result<()>;
}

struct AppDirectMessageDelivery {
    app_state: Arc<AppState>,
    token: CancellationToken,
}

#[async_trait]
impl DirectMessageDelivery for AppDirectMessageDelivery {
    async fn deliver(&self, event_id: EventId, recipient: PublicKey) -> Result<()> {
        event_handler::send_direct_message_notification(
            &self.app_state,
            event_id,
            &recipient,
            self.token.clone(),
        )
        .await
    }
}

/// Router state: the health endpoint reports both service identity and the
/// liveness of the delivery tasks.
#[derive(Clone)]
pub struct ServerState {
    service_pubkey: Option<String>,
    health: Arc<TaskHealth>,
    metrics: PrometheusHandle,
    internal_api_token: Option<String>,
    direct_message_delivery: Arc<dyn DirectMessageDelivery>,
}

impl ServerState {
    pub fn new(
        app_state: Arc<AppState>,
        health: Arc<TaskHealth>,
        metrics: PrometheusHandle,
        token: CancellationToken,
    ) -> Self {
        Self {
            service_pubkey: app_state.service_pubkey_hex(),
            health,
            metrics,
            internal_api_token: app_state.settings.server.internal_api_token.clone(),
            direct_message_delivery: Arc::new(AppDirectMessageDelivery { app_state, token }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectMessageRequest {
    event_id: String,
    recipient_pubkey: String,
    message_type: DirectMessageType,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DirectMessageType {
    ModerationNotice,
    ReportOutcome,
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

async fn direct_message(
    State(state): State<ServerState>,
    Json(request): Json<DirectMessageRequest>,
) -> Response {
    let event_id = match EventId::from_hex(&request.event_id) {
        Ok(event_id) => event_id,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid eventId"),
    };
    let recipient = match PublicKey::from_hex(&request.recipient_pubkey) {
        Ok(recipient) => recipient,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid recipientPubkey"),
    };

    tracing::info!(
        event_id = %event_id,
        recipient = %recipient,
        message_type = ?request.message_type,
        "Processing trusted direct-message push request"
    );

    match state
        .direct_message_delivery
        .deliver(event_id, recipient)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(event_id = %event_id, error = %error, "Direct-message push request failed");
            error.into_response()
        }
    }
}

async fn require_internal_api_token(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected_token) = state.internal_api_token.as_deref() else {
        tracing::error!("Internal push endpoint is disabled because no bearer token is configured");
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Internal push endpoint is not configured",
        );
    };

    if !has_bearer_token(request.headers(), expected_token) {
        return error_response(StatusCode::UNAUTHORIZED, "Unauthorized");
    }

    next.run(request).await
}

fn has_bearer_token(headers: &HeaderMap, expected_token: &str) -> bool {
    let Some(provided_token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };

    blake3::hash(provided_token.as_bytes()) == blake3::hash(expected_token.as_bytes())
}

fn error_response(status: StatusCode, message: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics))
        .route(
            "/internal/v1/direct-message",
            post(direct_message).route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_internal_api_token,
            )),
        )
        .with_state(state)
}

pub async fn run_server(
    app_state: Arc<AppState>,
    health: Arc<TaskHealth>,
    metrics: PrometheusHandle,
    token: CancellationToken,
) {
    let app = router(ServerState::new(
        Arc::clone(&app_state),
        health,
        metrics,
        token.clone(),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[derive(Default)]
    struct MockDelivery {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DirectMessageDelivery for MockDelivery {
        async fn deliver(&self, _event_id: EventId, _recipient: PublicKey) -> Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn server_state(health: Arc<TaskHealth>) -> ServerState {
        let metrics = PrometheusBuilder::new().build_recorder().handle();
        ServerState {
            service_pubkey: Some("pubkey-hex".to_string()),
            health,
            metrics,
            internal_api_token: Some("test-token".to_string()),
            direct_message_delivery: Arc::new(MockDelivery::default()),
        }
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
        assert_eq!(body["tasks"]["new_post_fanout"], true);
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
        let response = router(ServerState {
            service_pubkey: Some("pubkey-hex".to_string()),
            health: Arc::new(TaskHealth::new()),
            metrics: handle,
            internal_api_token: Some("test-token".to_string()),
            direct_message_delivery: Arc::new(MockDelivery::default()),
        })
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

    fn direct_message_request(token: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/internal/v1/direct-message")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn direct_message_requires_the_internal_bearer_token() {
        let response = router(server_state(Arc::new(TaskHealth::new())))
            .oneshot(direct_message_request(
                Some("wrong-token"),
                r#"{"eventId":"1111111111111111111111111111111111111111111111111111111111111111","recipientPubkey":"79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","messageType":"moderation_notice"}"#,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn direct_message_authenticates_before_parsing_the_body() {
        let response = router(server_state(Arc::new(TaskHealth::new())))
            .oneshot(direct_message_request(
                Some("wrong-token"),
                r#"{"messageType":"reaction"}"#,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn direct_message_rejects_unclassified_messages() {
        let response = router(server_state(Arc::new(TaskHealth::new())))
            .oneshot(direct_message_request(
                Some("test-token"),
                r#"{"eventId":"1111111111111111111111111111111111111111111111111111111111111111","recipientPubkey":"79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","messageType":"reaction"}"#,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn direct_message_delivers_a_valid_classified_request() {
        let delivery = Arc::new(MockDelivery::default());
        let state = ServerState {
            service_pubkey: Some("pubkey-hex".to_string()),
            health: Arc::new(TaskHealth::new()),
            metrics: PrometheusBuilder::new().build_recorder().handle(),
            internal_api_token: Some("test-token".to_string()),
            direct_message_delivery: delivery.clone(),
        };
        let response = router(state)
            .oneshot(direct_message_request(
                Some("test-token"),
                r#"{"eventId":"1111111111111111111111111111111111111111111111111111111111111111","recipientPubkey":"79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","messageType":"moderation_notice"}"#,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(delivery.calls.load(Ordering::Relaxed), 1);
    }
}
