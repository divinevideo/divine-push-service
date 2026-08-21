//! Regression test: a lagging broadcast receiver must not end the live listener.
//!
//! `notifications().recv()` returns a bounded `tokio` broadcast receiver.
//! Treating its recoverable `Lagged` variant as fatal ended the live loop, and
//! nothing restarts it — so a moment of back-pressure stopped every push for
//! the life of the process while `/health` kept returning 200 OK.

use divine_push_service::nostr_listener::{recv_action, RecvAction};
use tokio::sync::broadcast::error::RecvError;

#[test]
fn lagged_keeps_the_listener_running() {
    // The next recv() returns the oldest message still retained, so the
    // listener must keep going rather than give up.
    assert_eq!(recv_action(&RecvError::Lagged(1)), RecvAction::Continue);
    assert_eq!(recv_action(&RecvError::Lagged(4096)), RecvAction::Continue);
}

#[test]
fn closed_stops_the_listener() {
    // Every sender is gone; no further event can arrive.
    assert_eq!(recv_action(&RecvError::Closed), RecvAction::Stop);
}
