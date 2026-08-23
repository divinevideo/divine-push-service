use crate::{error::Result, models::FcmPayload};
use async_trait::async_trait;
use futures_util::{stream, FutureExt, StreamExt};
use gcp_auth::TokenProvider;
use reqwest::{header::HeaderMap, StatusCode};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, time::Duration};
use thiserror::Error;
use tracing;

const FCM_BATCH_CONCURRENCY: usize = 100;

/// Return a log-safe prefix without splitting a multibyte character.
pub(crate) fn token_prefix(token: &str) -> String {
    token.chars().take(8).collect()
}

/// OAuth2 scope required by the FCM v1 send endpoint.
const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";

/// Base URL of the FCM v1 API. Overridden in tests to point at a local server.
const FCM_BASE_URL: &str = "https://fcm.googleapis.com";

/// Bounds one FCM send end to end.
///
/// `reqwest` applies no request timeout by default. Without this, a peer that
/// completes the TLS handshake, accepts the request, and then never finishes the
/// response would park `send_single` forever — stalling the single event-handler
/// task while its supervision guard stays alive and `/health` keeps returning
/// 200. That is the same silent-outage shape this service just spent 59 hours in,
/// reached by a different route.
const FCM_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds connection establishment separately, so a blackholed address fails
/// fast instead of consuming the whole request budget.
const FCM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard ceiling on one `send_single`, covering everything it does — not just
/// the HTTP call.
///
/// `FCM_REQUEST_TIMEOUT` is a `reqwest` client setting, so it only starts once a
/// request exists. OAuth token acquisition happens first and runs on `gcp_auth`'s
/// own Hyper client, which sets no connect, read, or total timeout anywhere in
/// the crate. A GKE metadata server that accepts the refresh connection and never
/// answers would therefore hang the event-handler task forever with both critical
/// flags still green — the silent outage again, one layer further out.
///
/// Deliberately larger than `FCM_REQUEST_TIMEOUT` so the inner timeout normally
/// fires first and yields the more specific error; this is the backstop that
/// bounds whatever else the function grows to do.
const FCM_OPERATION_TIMEOUT: Duration = Duration::from_secs(45);
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);

fn build_http_client() -> Result<reqwest::Client, FcmError> {
    build_http_client_with(FCM_REQUEST_TIMEOUT, FCM_CONNECT_TIMEOUT)
}

/// Split out so tests exercise the production construction path with a short
/// timeout, rather than asserting against a client they built themselves.
fn build_http_client_with(
    timeout: Duration,
    connect_timeout: Duration,
) -> Result<reqwest::Client, FcmError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|e| FcmError::Initialization(format!("Failed to build HTTP client: {}", e)))
}

#[derive(Error, Debug, Clone)]
pub enum FcmError {
    #[error("Initialization error: {0}")]
    Initialization(String),
    #[error("FCM internal request error: {0}")]
    InternalRequest(String),
    #[error("FCM internal response error: {0}")]
    InternalResponse(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("FCM indicated token is not registered or invalid")]
    TokenNotRegistered,
    #[error("Retryable internal error. Retry after: {0:?}")]
    RetryableInternal(Duration),
    #[error("FCM internal error")]
    InternalError,
    #[error("Unknown FCM error: code={code}, hint={hint:?}")]
    Unknown { code: u16, hint: Option<String> },
}

impl FcmError {
    /// Stable, bounded reason used by the FCM failure metric.
    pub fn metric_reason(&self) -> &'static str {
        match self {
            Self::Initialization(_) => "initialization",
            Self::InternalRequest(_) => "internal_request",
            Self::InternalResponse(_) => "internal_response",
            Self::Unauthorized(_) => "unauthorized",
            Self::InvalidRequest(_) => "invalid_request",
            Self::TokenNotRegistered => "token_not_registered",
            Self::RetryableInternal(_) => "retryable_internal",
            Self::InternalError => "internal_error",
            Self::Unknown { .. } => "unknown",
        }
    }
}

/// The `errorCode` FCM returns in `error.details[]` for a token the device has
/// discarded. This is the signal that a stored token should be pruned.
const FCM_ERROR_CODE_UNREGISTERED: &str = "UNREGISTERED";
/// The legacy HTTP API's spelling. Matched only in the structured `errorCode`
/// field, never in free text, so prose can never trip it.
const FCM_ERROR_CODE_NOT_REGISTERED: &str = "NotRegistered";
const FCM_ERROR_CODE_SENDER_ID_MISMATCH: &str = "SENDER_ID_MISMATCH";

/// Pulls `error.message` out of an FCM v1 error body, falling back to the raw
/// body when it is not the JSON shape we expect.
fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string())
}

/// Pulls the FCM-specific `errorCode` out of `error.details[]`.
fn extract_fcm_error_code(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let details = value.get("error")?.get("details")?.as_array()?;
    details
        .iter()
        .find_map(|detail| detail.get("errorCode").and_then(|c| c.as_str()))
        .map(str::to_string)
}

/// True when the message is *narrowly* about the device token being dead.
///
/// This is a fallback for the documented 400 invalid-registration response; the
/// authoritative signal is the structured `errorCode`. The phrases must stay
/// specific to a registration token. Bare `unregistered` / `not registered`
/// were deliberately removed: this text can come from any upstream in the path,
/// and a proxy replying "upstream service is not registered" would otherwise
/// delete live tokens — in bulk, since such a failure hits every send at once.
fn message_indicates_dead_token(message: &str) -> bool {
    let lowered = message.to_lowercase();
    lowered.contains("invalid registration token")
        || lowered.contains("not a valid fcm registration token")
        || message.contains("BadDeviceToken")
}

/// Reads the retry delay FCM suggests alongside a 5xx.
///
/// The header name must be lowercase. Constructing it with an uppercase name is
/// what took the service down: `http`'s `HeaderName::from_static` rejects
/// uppercase (HTTP/2 requires lowercase) by panicking, so every FCM 5xx killed
/// the delivery task. See the `retry_after` tests below.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Maps an unsuccessful FCM response onto a typed error.
///
/// Unlike the library this replaced, the response body is consulted on *every*
/// status, not just 400. FCM reports a dead token as 404 with an `UNREGISTERED`
/// detail, so discarding non-400 bodies made dead tokens indistinguishable from
/// generic failures and left them un-prunable.
fn classify_error(status: StatusCode, headers: &HeaderMap, body: &str) -> FcmError {
    let message = extract_error_message(body);
    let error_code = extract_fcm_error_code(body);

    // 429 is FCM's explicit backpressure signal, and a large new-post fan-out is
    // exactly where it shows up. It carries the same "retry later" meaning as a
    // 5xx and honours the same `Retry-After`, so it must be retryable: a 429 that
    // sent nothing is unambiguous, and treating it as permanent strands the whole
    // page's watchers for the recipient-claim TTL.
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        return FcmError::RetryableInternal(
            parse_retry_after(headers).unwrap_or(DEFAULT_RETRY_AFTER),
        );
    }

    // Deliberately NOT keyed on the 404 status alone. `TokenNotRegistered`
    // deletes the token from Redis (`event_handler.rs`), so a bare 404 from a
    // misconfigured project path, a stray proxy, or any other non-token
    // `NOT_FOUND` would silently erase live device registrations. FCM always
    // reports a discarded token with an `UNREGISTERED` detail, so require
    // positive evidence: under-pruning is recoverable, over-pruning is not.
    if matches!(
        error_code.as_deref(),
        Some(FCM_ERROR_CODE_UNREGISTERED) | Some(FCM_ERROR_CODE_NOT_REGISTERED)
    ) || message_indicates_dead_token(&message)
    {
        return FcmError::TokenNotRegistered;
    }

    if matches!(
        error_code.as_deref(),
        Some(FCM_ERROR_CODE_SENDER_ID_MISMATCH)
    ) || message.contains("SENDER_ID_MISMATCH")
        || message.contains("sender id mismatch")
        || (message.contains("project") && message.contains("mismatch"))
    {
        return FcmError::InvalidRequest(format!("Project mismatch: {}", message));
    }

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => FcmError::Unauthorized(message),
        _ if status.is_client_error() => FcmError::InvalidRequest(message),
        _ => FcmError::Unknown {
            code: status.as_u16(),
            hint: Some(message),
        },
    }
}

// Define the trait for sending FCM messages
#[async_trait]
pub trait FcmSend: Send + Sync {
    async fn send_single(
        &self,
        token: &str,
        payload: FcmPayload,
    ) -> std::result::Result<(), FcmError>;
}

/// Sends to the FCM v1 REST API over `reqwest`, authenticating with Application
/// Default Credentials (GKE Workload Identity in production).
struct RealFcmClient {
    http: reqwest::Client,
    token_provider: Arc<dyn TokenProvider>,
    send_url: String,
}

impl RealFcmClient {
    async fn new(project_id: &str) -> Result<Self, FcmError> {
        let token_provider = gcp_auth::provider().await.map_err(|e| {
            FcmError::Initialization(format!(
                "Failed to resolve Google credentials for project {}: {}",
                project_id, e
            ))
        })?;

        Ok(Self::with_parts(
            build_http_client()?,
            token_provider,
            FCM_BASE_URL,
            project_id,
        ))
    }

    fn with_parts(
        http: reqwest::Client,
        token_provider: Arc<dyn TokenProvider>,
        base_url: &str,
        project_id: &str,
    ) -> Self {
        Self {
            http,
            token_provider,
            send_url: format!(
                "{}/v1/projects/{}/messages:send",
                base_url.trim_end_matches('/'),
                project_id
            ),
        }
    }
}

/// Builds the `apns` block of an FCM v1 message.
///
/// Returns raw JSON rather than typed structs; the shape is the APNs payload
/// documented by Apple and passed through by FCM verbatim.
fn build_apns_config(payload: &FcmPayload) -> Option<serde_json::Value> {
    // APNS config is transport-owned — FcmPayload.apns should never be set.
    debug_assert!(
        payload.apns.is_none(),
        "FcmPayload.apns must not be set; APNS config is built at transport level"
    );

    let data = payload.data.clone().unwrap_or_default();
    let title = payload
        .notification
        .as_ref()
        .and_then(|notification| notification.title.clone())
        .or_else(|| data.get("title").cloned());
    let body = payload
        .notification
        .as_ref()
        .and_then(|notification| notification.body.clone())
        .or_else(|| data.get("body").cloned());

    if title.is_some() || body.is_some() {
        // Alert push: the OS presents this single banner (NSE may enrich it via
        // mutable-content). Deliberately NO content-available — that flag wakes the
        // app's background isolate, which renders a *second*, duplicate banner. An
        // alert push reaches terminated iOS apps without it. See divine-push-service#20.
        let mut alert = serde_json::Map::new();
        if let Some(title) = title {
            alert.insert("title".to_string(), serde_json::Value::String(title));
        }
        if let Some(body) = body {
            alert.insert("body".to_string(), serde_json::Value::String(body));
        }

        // Custom data sits alongside `aps`. Filter title/body — they're already
        // in aps.alert.
        let mut apns_payload = json_object_from_data(
            data.into_iter()
                .filter(|(key, _)| key != "title" && key != "body"),
        );
        apns_payload.insert(
            "aps".to_string(),
            serde_json::json!({
                "alert": alert,
                "mutable-content": 1,
            }),
        );

        return Some(serde_json::json!({
            "payload": apns_payload,
            "headers": {
                "apns-push-type": "alert",
                "apns-priority": "10",
            },
        }));
    }

    if data.is_empty() {
        return None;
    }

    // Data-only: a silent background wake, which does require content-available.
    let mut apns_payload = json_object_from_data(data.into_iter());
    apns_payload.insert(
        "aps".to_string(),
        serde_json::json!({ "content-available": 1 }),
    );

    Some(serde_json::json!({
        "payload": apns_payload,
        "headers": {
            "apns-push-type": "background",
            "apns-priority": "5",
        },
    }))
}

/// Builds the `android` block for user-visible FCM messages.
///
/// High priority may wake a device from Doze, so reserve it for payloads that
/// contain notification copy and will produce a visible notification.
fn build_android_config(payload: &FcmPayload) -> Option<serde_json::Value> {
    let notification_has_alert = payload
        .notification
        .as_ref()
        .is_some_and(|notification| notification.title.is_some() || notification.body.is_some());
    let data_has_alert = payload
        .data
        .as_ref()
        .is_some_and(|data| data.contains_key("title") || data.contains_key("body"));

    (notification_has_alert || data_has_alert).then(|| serde_json::json!({ "priority": "high" }))
}

fn json_object_from_data(
    data: impl Iterator<Item = (String, String)>,
) -> serde_json::Map<String, serde_json::Value> {
    data.map(|(key, value)| (key, serde_json::Value::String(value)))
        .collect()
}

#[async_trait]
impl FcmSend for RealFcmClient {
    /// Sends a notification payload to a single FCM token via the FCM v1 API.
    ///
    /// Wrapped in a hard operation timeout: no path through here may block the
    /// event-handler task indefinitely.
    async fn send_single(
        &self,
        token: &str,
        payload: FcmPayload,
    ) -> std::result::Result<(), FcmError> {
        let prefix = token_prefix(token);

        match tokio::time::timeout(FCM_OPERATION_TIMEOUT, self.send_inner(token, payload)).await {
            Ok(result) => result,
            Err(_) => {
                let error = FcmError::InternalRequest(format!(
                    "FCM send exceeded {:?}",
                    FCM_OPERATION_TIMEOUT
                ));
                tracing::error!("FCM send failed for token prefix {}: {}", prefix, error);
                Err(error)
            }
        }
    }
}

impl RealFcmClient {
    async fn send_inner(
        &self,
        token: &str,
        payload: FcmPayload,
    ) -> std::result::Result<(), FcmError> {
        let prefix = token_prefix(token);
        let apns = build_apns_config(&payload);
        let android = build_android_config(&payload);

        let mut message = serde_json::Map::new();
        message.insert(
            "token".to_string(),
            serde_json::Value::String(token.to_string()),
        );

        // Support both notification+data and data-only messages.
        if let Some(notification) = payload.notification {
            message.insert(
                "notification".to_string(),
                serde_json::to_value(notification).map_err(|e| {
                    FcmError::InternalRequest(format!("Failed to serialize notification: {}", e))
                })?,
            );
        }
        if let Some(data) = payload.data {
            message.insert(
                "data".to_string(),
                serde_json::to_value(data).map_err(|e| {
                    FcmError::InternalRequest(format!("Failed to serialize data: {}", e))
                })?,
            );
        }
        if let Some(apns) = apns {
            message.insert("apns".to_string(), apns);
        }
        if let Some(android) = android {
            message.insert("android".to_string(), android);
        }

        let access_token =
            self.token_provider.token(&[FCM_SCOPE]).await.map_err(|e| {
                FcmError::Unauthorized(format!("Failed to obtain access token: {}", e))
            })?;

        tracing::info!(
            "Sending simplified FCM request for token prefix {}...",
            prefix
        );

        let response = self
            .http
            .post(&self.send_url)
            .bearer_auth(access_token.as_str())
            .json(&serde_json::json!({ "message": message }))
            .send()
            .await
            .map_err(|e| FcmError::InternalRequest(format!("FCM request failed: {}", e)))?;

        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            tracing::info!("FCM send successful for token prefix {}", prefix);
            return Ok(());
        }

        tracing::debug!(
            "FCM error response for token prefix {}: status={} body={}",
            prefix,
            status,
            body
        );

        let error = classify_error(status, &headers, &body);
        tracing::error!("FCM send failed for token prefix {}: {}", prefix, error);
        Err(error)
    }
}

// The public FcmClient now holds a trait object
pub struct FcmClient {
    // Changed client type to a Boxed trait object
    client: Box<dyn FcmSend>,
}

impl FcmClient {
    /// Create a new FCM client for the given project ID
    pub async fn new(project_id: &str) -> Result<Self, FcmError> {
        let real_client = RealFcmClient::new(project_id).await?;
        tracing::info!("Initialized FCM client for project: {}", project_id);
        Ok(FcmClient {
            client: Box::new(real_client),
        })
    }

    // Add a constructor for injecting a mock/custom implementation (for testing)
    pub fn new_with_impl(client_impl: Box<dyn FcmSend>) -> Self {
        FcmClient {
            client: client_impl,
        }
    }

    /// Sends a notification payload to a batch of tokens.
    /// Delegates to the underlying FcmSend implementation's send_single.
    pub async fn send_batch(
        &self,
        tokens: &[String],
        payload: FcmPayload,
    ) -> HashMap<String, std::result::Result<(), FcmError>> {
        let results = stream::iter(tokens.iter().cloned())
            .map(|token| {
                let payload = payload.clone();
                async move {
                    crate::metrics::fcm_send_attempted();
                    // Defence in depth, retained from #42. The specific panic it
                    // was written for is gone — `firebase-messaging-rs` read the
                    // `Retry-After` header via `HeaderName::from_static`, which
                    // panics on uppercase, and that dependency has been replaced.
                    // The guard stays because it protects any `FcmSend` impl, and
                    // a panic here would unwind into the event-handler task and
                    // stop all delivery for the life of the process.
                    let result = AssertUnwindSafe(self.client.send_single(&token, payload))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            tracing::error!(
                                "FCM send panicked for token prefix {} - dropping this \
                                 notification and continuing",
                                token_prefix(&token)
                            );
                            Err(FcmError::InternalResponse(
                                "FCM send panicked while handling the response".to_string(),
                            ))
                        });
                    match &result {
                        Ok(()) => crate::metrics::fcm_send_succeeded(),
                        Err(error) => crate::metrics::fcm_send_failed(error.metric_reason()),
                    }
                    (token, result)
                }
            })
            .buffer_unordered(FCM_BATCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        // Note: We are not returning a Result here anymore, but a HashMap of results.
        // The original code returned Result<HashMap<...>, FcmError> which seemed incorrect
        // as individual sends could fail without the whole batch failing.
        // If a top-level error IS needed (e.g., impossible to even start sending),
        // this signature might need adjustment, but usually, per-token results are desired.
        results.into_iter().collect()
    }

    /// Sends a notification payload to a single FCM token.
    /// Delegates directly to the underlying FcmSend implementation.
    pub async fn send_single(
        &self,
        token: &str,
        payload: FcmPayload,
    ) -> std::result::Result<(), FcmError> {
        // Delegate to the trait object's method
        self.client.send_single(token, payload).await
    }
}

// Define the mock FCM sender struct (Now public and outside cfg(test))
#[derive(Clone, Default)]
pub struct MockFcmSender {
    // Store sent messages: Arc<Mutex<Vec<(token, payload)>>>
    sent_messages: Arc<Mutex<Vec<(String, FcmPayload)>>>, // Made fields crate-public for direct access if needed, or keep private and use methods
    // Optional: Simulate specific errors for certain tokens
    error_tokens: Arc<Mutex<HashMap<String, FcmError>>>,
}

// Make methods public
impl MockFcmSender {
    pub fn new() -> Self {
        Self {
            sent_messages: Arc::new(Mutex::new(Vec::new())),
            error_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // Helper to retrieve sent messages for assertions
    pub fn get_sent_messages(&self) -> Vec<(String, FcmPayload)> {
        self.sent_messages.lock().unwrap().clone()
    }

    // Helper to simulate errors for specific tokens
    pub fn set_error_for_token(&self, token: &str, error: FcmError) {
        self.error_tokens
            .lock()
            .unwrap()
            .insert(token.to_string(), error);
    }

    // Helper to clear recorded messages and errors (useful between tests)
    pub fn clear(&self) {
        self.sent_messages.lock().unwrap().clear();
        self.error_tokens.lock().unwrap().clear();
    }
}

#[async_trait]
impl FcmSend for MockFcmSender {
    async fn send_single(
        &self,
        token: &str,
        payload: FcmPayload,
    ) -> std::result::Result<(), FcmError> {
        // Check if this token should simulate an error
        if let Some(error) = self.error_tokens.lock().unwrap().get(token) {
            tracing::warn!(
                "MockFcmSender: Simulating error {:?} for token prefix {}",
                error,
                token_prefix(token)
            );
            return Err(error.clone()); // Return the predefined error
        }

        // Otherwise, record the message
        tracing::info!(
            "MockFcmSender: Recording send for token prefix {}...",
            token_prefix(token)
        );
        let mut messages = self.sent_messages.lock().unwrap();
        messages.push((token.to_string(), payload));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*; // Import items from parent module (fcm_sender)
    use crate::models::{FcmNotification, FcmPayload};

    use tokio; // Ensure tokio is available for async tests

    // Example test demonstrating how to use the mock
    #[tokio::test]
    async fn test_mock_fcm_sender_single_send() {
        // Ensure FcmPayload derives PartialEq and Clone for this assertion
        let mock_sender = MockFcmSender::new(); // Create instance directly
                                                // Pass a boxed clone to the client
        let fcm_client = FcmClient::new_with_impl(Box::new(mock_sender.clone()));

        let token = "test_token_1";
        let payload = FcmPayload {
            notification: Some(FcmNotification {
                title: Some("Test Title".to_string()),
                body: Some("Test Body".to_string()),
            }),
            data: None, // Add data if needed
            android: None,
            webpush: None,
            apns: None,
        };

        // Use the FcmClient (which internally uses the mock)
        let result = fcm_client.send_single(token, payload.clone()).await;

        assert!(result.is_ok());

        // Verify the message was recorded by the mock (use original instance)
        let sent = mock_sender.get_sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, token);
        assert_eq!(sent[0].1, payload);
    }

    #[tokio::test]
    async fn test_mock_fcm_sender_batch_send() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);
        // Ensure FcmPayload derives PartialEq and Clone
        let mock_sender = MockFcmSender::new(); // Create instance directly
                                                // Pass a boxed clone to the client
        let fcm_client = FcmClient::new_with_impl(Box::new(mock_sender.clone()));

        let tokens = vec!["token1".to_string(), "token2".to_string()];
        let payload = FcmPayload {
            notification: Some(FcmNotification {
                title: Some("Batch Title".to_string()),
                body: Some("Batch Body".to_string()),
            }),
            data: None,
            android: None,
            webpush: None,
            apns: None,
        };

        // Simulate an error for one token (use original instance)
        mock_sender.set_error_for_token("token2", FcmError::TokenNotRegistered);

        // Use the FcmClient's batch send (which calls send_single repeatedly)
        let results = fcm_client.send_batch(&tokens, payload.clone()).await;

        // Verify results map
        assert_eq!(results.len(), 2);
        assert!(results.get("token1").unwrap().is_ok());
        assert!(matches!(
            results.get("token2").unwrap(),
            Err(FcmError::TokenNotRegistered)
        ));

        // Verify only the successful message was recorded by the mock (use original instance)
        let sent = mock_sender.get_sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "token1");
        assert_eq!(sent[0].1, payload);

        handle.run_upkeep();
        let rendered = handle.render();
        assert!(
            rendered.contains("push_fcm_sends_attempted_total 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("push_fcm_sends_succeeded_total 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"push_fcm_sends_failed_total{reason="token_not_registered"} 1"#),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn test_mock_fcm_sender_error_simulation() {
        // Ensure FcmPayload derives PartialEq and Clone
        let mock_sender = MockFcmSender::new(); // Create instance directly
                                                // Pass a boxed clone to the client
        let fcm_client = FcmClient::new_with_impl(Box::new(mock_sender.clone()));

        let error_token = "error_token";
        let simulated_error = FcmError::InternalError; // Example error
                                                       // Set error using original instance
        mock_sender.set_error_for_token(error_token, simulated_error.clone());

        let payload = FcmPayload {
            notification: Some(FcmNotification {
                title: Some("Error Test".to_string()),
                body: Some("Error Body".to_string()),
            }),
            data: None,
            android: None,
            webpush: None,
            apns: None,
        };

        // Send to the token that should produce an error
        let result = fcm_client.send_single(error_token, payload.clone()).await;

        // Assert that the expected error was returned
        assert!(result.is_err());
        // Use matches! macro for better error checking if FcmError doesn't impl PartialEq
        assert!(matches!(result.unwrap_err(), FcmError::InternalError));

        // Assert that no message was recorded for the error token (use original instance)
        let sent = mock_sender.get_sent_messages();
        assert!(sent.is_empty());
    }

    #[test]
    fn test_build_apns_config_creates_alert_payload_from_data_only_message() {
        let mut data = std::collections::HashMap::new();
        data.insert("title".to_string(), "New like".to_string());
        data.insert("body".to_string(), "Alice liked your post".to_string());
        data.insert("eventId".to_string(), "abc123".to_string());

        let payload = FcmPayload {
            notification: None,
            data: Some(data),
            android: None,
            webpush: None,
            apns: None,
        };

        let apns_config = build_apns_config(&payload).expect("apns config should exist");

        let json = serde_json::to_value(&apns_config).expect("serialization should succeed");
        let expected = serde_json::json!({
            "payload": {
                "aps": {
                    "alert": {
                        "title": "New like",
                        "body": "Alice liked your post"
                    },
                    "mutable-content": 1
                },
                "eventId": "abc123"
            },
            "headers": {
                "apns-push-type": "alert",
                "apns-priority": "10"
            }
        });
        assert_eq!(json, expected);
    }

    #[test]
    fn test_build_apns_config_uses_background_transport_without_alert_fields() {
        let mut data = std::collections::HashMap::new();
        data.insert("eventId".to_string(), "abc123".to_string());

        let payload = FcmPayload {
            notification: None,
            data: Some(data),
            android: None,
            webpush: None,
            apns: None,
        };

        let apns_config = build_apns_config(&payload).expect("apns config should exist");

        let json = serde_json::to_value(&apns_config).expect("serialization should succeed");
        let expected = serde_json::json!({
            "payload": {
                "aps": {
                    "content-available": 1
                },
                "eventId": "abc123"
            },
            "headers": {
                "apns-push-type": "background",
                "apns-priority": "5"
            }
        });
        assert_eq!(json, expected);
    }

    // ---------------------------------------------------------------------
    // Error classification
    //
    // The library this replaced panicked on every FCM 5xx while reading the
    // retry hint, which killed the delivery task and silently stopped all
    // pushes for 59 hours. It also discarded response bodies on non-400
    // statuses, so a 404 `UNREGISTERED` was indistinguishable from any other
    // failure and dead tokens could never be pruned.
    // ---------------------------------------------------------------------

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn retry_after_is_read_regardless_of_header_casing() {
        // FCM sends `Retry-After`. Looking it up must not depend on casing —
        // and must never be done with `HeaderName::from_static`, which panics
        // on uppercase input.
        for name in ["retry-after", "Retry-After", "RETRY-AFTER"] {
            assert_eq!(
                parse_retry_after(&header_map(&[(name, "120")])),
                Some(Duration::from_secs(120)),
                "failed for header name {name}"
            );
        }
    }

    #[test]
    fn retry_after_absent_or_unparseable_is_none() {
        assert_eq!(parse_retry_after(&header_map(&[])), None);
        // An HTTP-date form is valid per RFC but unsupported; it must degrade
        // rather than panic or mis-parse.
        assert_eq!(
            parse_retry_after(&header_map(&[(
                "retry-after",
                "Wed, 21 Oct 2026 07:28:00 GMT"
            )])),
            None
        );
    }

    #[test]
    fn server_error_with_retry_after_is_retryable() {
        let error = classify_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &header_map(&[("Retry-After", "30")]),
            r#"{"error":{"code":503,"message":"The service is currently unavailable."}}"#,
        );
        assert!(matches!(error, FcmError::RetryableInternal(d) if d == Duration::from_secs(30)));
    }

    #[test]
    fn server_error_without_retry_after_uses_default_retry_delay() {
        let error = classify_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &header_map(&[]),
            r#"{"error":{"code":500,"message":"Internal error"}}"#,
        );
        assert!(matches!(error, FcmError::RetryableInternal(d) if d == DEFAULT_RETRY_AFTER));
    }

    #[test]
    fn too_many_requests_is_retryable_and_honors_retry_after() {
        let error = classify_error(
            StatusCode::TOO_MANY_REQUESTS,
            &header_map(&[("Retry-After", "12")]),
            r#"{"error":{"code":429,"message":"Quota exceeded."}}"#,
        );
        assert!(matches!(error, FcmError::RetryableInternal(d) if d == Duration::from_secs(12)));
    }

    #[test]
    fn too_many_requests_preserves_an_hour_retry_after() {
        let error = classify_error(
            StatusCode::TOO_MANY_REQUESTS,
            &header_map(&[("Retry-After", "3600")]),
            r#"{"error":{"code":429,"message":"Quota exceeded."}}"#,
        );
        assert!(matches!(error, FcmError::RetryableInternal(d) if d == Duration::from_secs(3_600)));
    }

    #[test]
    fn too_many_requests_without_retry_after_uses_default_retry_delay() {
        let error = classify_error(
            StatusCode::TOO_MANY_REQUESTS,
            &header_map(&[]),
            r#"{"error":{"code":429,"message":"Quota exceeded."}}"#,
        );
        assert!(matches!(error, FcmError::RetryableInternal(d) if d == DEFAULT_RETRY_AFTER));
    }

    #[test]
    fn not_found_with_unregistered_detail_marks_token_dead() {
        let error = classify_error(
            StatusCode::NOT_FOUND,
            &header_map(&[]),
            r#"{"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND","details":[{"@type":"type.googleapis.com/google.firebase.fcm.v1.FcmError","errorCode":"UNREGISTERED"}]}}"#,
        );
        assert!(matches!(error, FcmError::TokenNotRegistered));
    }

    /// `TokenNotRegistered` deletes the token from Redis, so it must require
    /// positive evidence that the token is dead. A bare 404 — wrong project
    /// path, stray proxy, any non-token NOT_FOUND — would otherwise erase every
    /// live registration it touched.
    #[test]
    fn bare_not_found_without_fcm_detail_does_not_prune_the_token() {
        let error = classify_error(
            StatusCode::NOT_FOUND,
            &header_map(&[]),
            "<html><body>404 Not Found</body></html>",
        );
        assert!(
            !matches!(error, FcmError::TokenNotRegistered),
            "a bare 404 must not delete a live token, got {error:?}"
        );

        // Same for a well-formed Google error that is not about the token.
        let error = classify_error(
            StatusCode::NOT_FOUND,
            &header_map(&[]),
            r#"{"error":{"code":404,"message":"Method not found.","status":"NOT_FOUND"}}"#,
        );
        assert!(
            !matches!(error, FcmError::TokenNotRegistered),
            "a routing 404 must not delete a live token, got {error:?}"
        );
    }

    /// The dead-token heuristic reads free text that any hop in the path can
    /// produce. Generic phrasing must not be treated as proof the device token
    /// is dead — an infrastructure failure hits every send at once, so a false
    /// positive here erases registrations in bulk.
    #[test]
    fn generic_upstream_text_does_not_prune_the_token() {
        for body in [
            "upstream service is not registered",
            r#"{"error":{"code":403,"message":"Caller is not registered for this API"}}"#,
            "<html><body>backend unregistered</body></html>",
            r#"{"error":{"code":502,"message":"gateway target not registered"}}"#,
        ] {
            let error = classify_error(StatusCode::BAD_REQUEST, &header_map(&[]), body);
            assert!(
                !matches!(error, FcmError::TokenNotRegistered),
                "generic text must not delete a live token: {body}"
            );
        }
    }

    /// The structured field stays authoritative, including the legacy spelling.
    #[test]
    fn legacy_not_registered_error_code_still_prunes() {
        let error = classify_error(
            StatusCode::BAD_REQUEST,
            &header_map(&[]),
            r#"{"error":{"code":400,"message":"Bad Request","details":[{"errorCode":"NotRegistered"}]}}"#,
        );
        assert!(matches!(error, FcmError::TokenNotRegistered));
    }

    #[test]
    fn invalid_argument_naming_the_token_marks_token_dead() {
        let error = classify_error(
            StatusCode::BAD_REQUEST,
            &header_map(&[]),
            r#"{"error":{"code":400,"message":"The registration token is not a valid FCM registration token"}}"#,
        );
        assert!(matches!(error, FcmError::TokenNotRegistered));
    }

    #[test]
    fn sender_id_mismatch_is_reported_as_project_mismatch() {
        let error = classify_error(
            StatusCode::FORBIDDEN,
            &header_map(&[]),
            r#"{"error":{"code":403,"message":"SenderId mismatch","details":[{"errorCode":"SENDER_ID_MISMATCH"}]}}"#,
        );
        match error {
            FcmError::InvalidRequest(message) => assert!(message.starts_with("Project mismatch:")),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn client_error_body_is_preserved_rather_than_discarded() {
        // The old library collapsed every non-retryable 4xx to "Unknown invalid
        // request (no details provided)", losing the reason entirely. (429 is no
        // longer an example here: it is now retryable backpressure, not a
        // permanent client error.)
        let error = classify_error(
            StatusCode::BAD_REQUEST,
            &header_map(&[]),
            r#"{"error":{"code":400,"message":"Invalid value at 'message.token'","status":"INVALID_ARGUMENT"}}"#,
        );
        match error {
            FcmError::InvalidRequest(message) => {
                assert!(message.contains("Invalid value"), "got: {message}")
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn unauthorized_is_typed_as_such() {
        let error = classify_error(
            StatusCode::UNAUTHORIZED,
            &header_map(&[]),
            r#"{"error":{"code":401,"message":"Request had invalid authentication credentials."}}"#,
        );
        assert!(matches!(error, FcmError::Unauthorized(_)));
    }

    #[test]
    fn non_json_body_falls_back_to_the_raw_text() {
        let error = classify_error(
            StatusCode::BAD_REQUEST,
            &header_map(&[]),
            "<html>502 Bad Gateway</html>",
        );
        match error {
            FcmError::InvalidRequest(message) => assert!(message.contains("Bad Gateway")),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // End-to-end send against a stub FCM endpoint
    // ---------------------------------------------------------------------

    struct StubTokenProvider;

    #[async_trait]
    impl TokenProvider for StubTokenProvider {
        async fn token(
            &self,
            _scopes: &[&str],
        ) -> std::result::Result<Arc<gcp_auth::Token>, gcp_auth::Error> {
            let token = serde_json::from_value(serde_json::json!({
                "access_token": "stub-access-token",
                "expires_in": 3600,
            }))
            .expect("stub token should deserialize");
            Ok(Arc::new(token))
        }

        async fn project_id(&self) -> std::result::Result<Arc<str>, gcp_auth::Error> {
            Ok(Arc::from("test-project"))
        }
    }

    type CapturedRequests = Arc<Mutex<Vec<(serde_json::Value, Option<String>)>>>;

    #[derive(Clone)]
    struct StubConfig {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        captured: CapturedRequests,
    }

    async fn stub_handler(
        axum::extract::State(config): axum::extract::State<StubConfig>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> axum::response::Response {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        config.captured.lock().unwrap().push((json, auth));

        let mut builder = axum::response::Response::builder().status(config.status);
        for (name, value) in &config.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
            .body(axum::body::Body::from(config.body.clone()))
            .unwrap()
    }

    /// Spawns a local stand-in for the FCM endpoint and returns a client
    /// pointed at it, plus the list of requests it received.
    async fn stub_fcm(
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (RealFcmClient, CapturedRequests) {
        let captured: CapturedRequests = Arc::new(Mutex::new(Vec::new()));
        let config = StubConfig {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: body.to_string(),
            captured: Arc::clone(&captured),
        };

        let app = axum::Router::new()
            .fallback(stub_handler)
            .with_state(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = RealFcmClient::with_parts(
            reqwest::Client::new(),
            Arc::new(StubTokenProvider),
            &format!("http://{addr}"),
            "test-project",
        );
        (client, captured)
    }

    fn social_payload() -> FcmPayload {
        let mut data = std::collections::HashMap::new();
        data.insert("eventId".to_string(), "abc123".to_string());
        data.insert("title".to_string(), "New like".to_string());
        data.insert("body".to_string(), "Alice liked your post".to_string());
        FcmPayload {
            notification: None,
            data: Some(data),
            android: None,
            webpush: None,
            apns: None,
        }
    }

    #[tokio::test]
    async fn send_single_posts_a_high_priority_data_only_social_message() {
        let (client, captured) =
            stub_fcm(200, &[], r#"{"name":"projects/test-project/messages/1"}"#).await;

        client
            .send_single("device-token-1", social_payload())
            .await
            .expect("send should succeed");

        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        let (body, auth) = &requests[0];

        assert_eq!(auth.as_deref(), Some("Bearer stub-access-token"));
        assert_eq!(body["message"]["token"], "device-token-1");
        assert!(body["message"].get("notification").is_none());
        assert_eq!(body["message"]["android"]["priority"], "high");
        assert_eq!(body["message"]["data"]["eventId"], "abc123");
        assert_eq!(body["message"]["data"]["title"], "New like");
        assert_eq!(
            body["message"]["apns"]["headers"]["apns-push-type"],
            "alert"
        );
        // The duplicate-banner guard from divine-push-service#20.
        assert!(body["message"]["apns"]["payload"]["aps"]
            .get("content-available")
            .is_none());
    }

    #[tokio::test]
    async fn send_single_does_not_use_high_priority_for_silent_data() {
        let (client, captured) =
            stub_fcm(200, &[], r#"{"name":"projects/test-project/messages/1"}"#).await;
        let payload = FcmPayload {
            notification: None,
            data: Some(std::collections::HashMap::from([(
                "eventId".to_string(),
                "abc123".to_string(),
            )])),
            android: None,
            webpush: None,
            apns: None,
        };

        client
            .send_single("device-token-1", payload)
            .await
            .expect("send should succeed");

        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        let (body, _) = &requests[0];

        assert!(body["message"].get("android").is_none());
        assert_eq!(
            body["message"]["apns"]["headers"]["apns-push-type"],
            "background"
        );
    }

    #[tokio::test]
    async fn send_single_preserves_notification_payload() {
        let (client, captured) =
            stub_fcm(200, &[], r#"{"name":"projects/test-project/messages/1"}"#).await;
        let payload = FcmPayload {
            notification: Some(FcmNotification {
                title: Some("New like".to_string()),
                body: Some("Alice liked your post".to_string()),
            }),
            data: None,
            android: None,
            webpush: None,
            apns: None,
        };

        client
            .send_single("device-token-1", payload)
            .await
            .expect("send should succeed");

        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        let (body, _) = &requests[0];

        assert_eq!(body["message"]["notification"]["title"], "New like");
        assert_eq!(body["message"]["android"]["priority"], "high");
    }

    /// The exact production failure: FCM returns 503 with a `Retry-After`
    /// header. This previously panicked the delivery task.
    #[tokio::test]
    async fn send_single_survives_a_server_error_with_retry_after() {
        let (client, _captured) = stub_fcm(
            503,
            &[("Retry-After", "42")],
            r#"{"error":{"code":503,"message":"The service is currently unavailable."}}"#,
        )
        .await;

        let result = client.send_single("device-token-1", social_payload()).await;

        match result {
            Err(FcmError::RetryableInternal(delay)) => {
                assert_eq!(delay, Duration::from_secs(42))
            }
            other => panic!("expected RetryableInternal, got {other:?}"),
        }
    }

    /// A peer that accepts the request and never answers must not park the
    /// event-handler task forever. That would stall delivery while the
    /// supervision guard stays alive and `/health` keeps returning 200 — the
    /// same silent-outage shape, reached without a panic.
    #[tokio::test]
    async fn send_single_times_out_instead_of_hanging_forever() {
        let app = axum::Router::new().fallback(|| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            axum::http::StatusCode::OK
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = RealFcmClient::with_parts(
            build_http_client_with(Duration::from_millis(300), Duration::from_millis(300))
                .expect("client should build"),
            Arc::new(StubTokenProvider),
            &format!("http://{addr}"),
            "test-project",
        );

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            client.send_single("device-token-1", social_payload()),
        )
        .await;

        match outcome {
            Ok(Err(FcmError::InternalRequest(_))) => {}
            Ok(other) => panic!("expected a timeout error, got {other:?}"),
            Err(_) => panic!("send_single outlived its own request timeout"),
        }
    }

    /// `gcp_auth` sets no timeout anywhere in the crate, so a stalled metadata
    /// token refresh returns before any reqwest request exists — meaning
    /// `FCM_REQUEST_TIMEOUT` never starts. Only the operation-level ceiling
    /// saves the event-handler task here.
    #[tokio::test(start_paused = true)]
    async fn send_single_times_out_when_token_acquisition_hangs() {
        struct HangingTokenProvider;

        #[async_trait]
        impl TokenProvider for HangingTokenProvider {
            async fn token(
                &self,
                _scopes: &[&str],
            ) -> std::result::Result<Arc<gcp_auth::Token>, gcp_auth::Error> {
                // Never resolves, exactly like a metadata server that accepts
                // the connection and never answers.
                std::future::pending().await
            }

            async fn project_id(&self) -> std::result::Result<Arc<str>, gcp_auth::Error> {
                Ok(Arc::from("test-project"))
            }
        }

        let client = RealFcmClient::with_parts(
            build_http_client().expect("client should build"),
            Arc::new(HangingTokenProvider),
            "http://127.0.0.1:1",
            "test-project",
        );

        // Paused clock: this advances virtual time, it does not really wait.
        let result = client.send_single("device-token-1", social_payload()).await;

        match result {
            Err(FcmError::InternalRequest(message)) => {
                assert!(message.contains("exceeded"), "got: {message}")
            }
            other => panic!("expected the operation timeout to fire, got {other:?}"),
        }
    }

    #[test]
    fn operation_timeout_backstops_the_request_timeout() {
        assert!(
            FCM_OPERATION_TIMEOUT > FCM_REQUEST_TIMEOUT,
            "the inner timeout must normally fire first and give the better error"
        );
    }

    #[test]
    fn production_http_client_is_built_with_bounded_timeouts() {
        // Guards the constants themselves: an unbounded client is the defect.
        assert!(build_http_client().is_ok());
        assert!(FCM_REQUEST_TIMEOUT > Duration::ZERO);
        assert!(FCM_CONNECT_TIMEOUT <= FCM_REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn send_single_reports_a_dead_token_as_unregistered() {
        let (client, _captured) = stub_fcm(
            404,
            &[],
            r#"{"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND","details":[{"errorCode":"UNREGISTERED"}]}}"#,
        )
        .await;

        let result = client.send_single("dead-token", social_payload()).await;
        assert!(matches!(result, Err(FcmError::TokenNotRegistered)));
    }
}
