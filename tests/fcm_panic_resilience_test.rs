//! Regression test: a panicking FCM send must not abort the whole batch.
//!
//! `send_batch` runs on the event-handler task. While that task was
//! unsupervised and its `JoinError` discarded, one panicking send stopped ALL
//! notification delivery for the life of the process while `/health` kept
//! returning 200 OK.
//!
//! Both halves of that outage are fixed outside this file: the panic itself
//! came from `firebase-messaging-rs`, which read the `Retry-After` header on
//! every FCM 5xx via `HeaderName::from_static("Retry-After")` while `http`'s
//! `from_static` panics on uppercase bytes, and that dependency is gone; and
//! `TaskTracker` now takes the process down when a critical task dies.
//!
//! What this test still guards is the layer between them: `send_batch` must
//! contain a panic from *any* `FcmSend` implementation rather than let it
//! unwind into the caller and take the siblings with it.

use async_trait::async_trait;
use divine_push_service::fcm_sender::{FcmClient, FcmError, FcmSend};
use divine_push_service::models::FcmPayload;

/// Panics for one specific token, succeeds for every other.
struct PanickingFcmSender {
    panic_on: String,
}

#[async_trait]
impl FcmSend for PanickingFcmSender {
    async fn send_single(
        &self,
        token: &str,
        _payload: FcmPayload,
    ) -> std::result::Result<(), FcmError> {
        if token == self.panic_on {
            panic!("simulated upstream panic while handling an FCM 5xx");
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_panicking_send_is_contained_and_does_not_abort_the_batch() {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .with_test_writer()
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let client = FcmClient::new_with_impl(Box::new(PanickingFcmSender {
        panic_on: "1234567é-bad".to_string(),
    }));

    let tokens = vec![
        "good-token-1".to_string(),
        "1234567é-bad".to_string(),
        "good-token-2".to_string(),
    ];

    // Before the fix this unwound into the caller and killed the task.
    let results = client.send_batch(&tokens, FcmPayload::default()).await;

    assert_eq!(results.len(), 3, "every token must get a result");

    let bad = results
        .get("1234567é-bad")
        .expect("panicking token result missing");
    assert!(bad.is_err(), "panicking send should surface as an error");

    // Crucially, the other recipients still get their notification.
    assert!(
        results.get("good-token-1").expect("missing").is_ok(),
        "a panic on one token must not fail its siblings"
    );
    assert!(
        results.get("good-token-2").expect("missing").is_ok(),
        "a panic on one token must not fail its siblings"
    );
}
