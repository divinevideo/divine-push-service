//! Parsing and validation of the `liveActivity` object in a decrypted kind 3079 payload.

use crate::crypto::TokenPayload;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// Longest window the app may request, in seconds.
const MAX_WINDOW_SECS: u64 = 3600;
/// Fewest and most post event ids (an edited post carries several).
const MAX_EVENT_IDS: usize = 10;

/// Whether the app wants updates (`active`) or has ended the activity (`ended`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Active,
    Ended,
}

/// A validated Live Activity registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveActivityRegistration {
    /// Lowercase hex APNs Live Activity push token. Never log it whole.
    pub activity_token: String,
    /// `<kind>:<author pubkey hex>:<d-tag>` of the post.
    pub post_coordinate: String,
    pub post_event_ids: Vec<String>,
    pub posted_at: u64,
    pub window_ends_at: u64,
    pub show_first_run_note: bool,
    pub state: ActivityState,
}

/// Why a `liveActivity` object was rejected. Display text never includes a token.
#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("liveActivity is malformed: {0}")]
    Malformed(String),
    #[error("activityPushToken must be 32 to 512 lowercase hex characters")]
    BadActivityToken,
    #[error("postCoordinate is not <kind>:<64 hex>:<d>: {}", clip(.0))]
    BadCoordinate(String),
    #[error("postCoordinate author {coordinate_author} is not the signer {signer}")]
    CoordinateAuthorMismatch {
        coordinate_author: String,
        signer: String,
    },
    #[error(
        "postEventIds must hold 1 to 10 entries of 64 lowercase hex characters: {}",
        clip(.0)
    )]
    BadEventIds(String),
    #[error("windowEndsAt must be after postedAt by at most 3600 seconds")]
    BadWindow,
    #[error("windowEndsAt is more than an hour plus the allowed clock skew in the future")]
    WindowTooFarAhead,
    #[error("state must be \"active\" or \"ended\"")]
    BadState,
}

/// Most characters of client-supplied text an error's Display shows; the text
/// reaches warn logs.
const MAX_ECHOED_CHARS: usize = 80;

/// The first [`MAX_ECHOED_CHARS`] characters of `s`, cut on a char boundary,
/// with `…` appended when anything was cut.
fn clip(s: &str) -> String {
    match s.char_indices().nth(MAX_ECHOED_CHARS) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

#[derive(Deserialize)]
struct Wire {
    #[serde(rename = "activityPushToken")]
    activity_push_token: String,
    #[serde(rename = "postCoordinate")]
    post_coordinate: String,
    #[serde(rename = "postEventIds")]
    post_event_ids: Vec<String>,
    #[serde(rename = "postedAt")]
    posted_at: u64,
    #[serde(rename = "windowEndsAt")]
    window_ends_at: u64,
    #[serde(rename = "showFirstRunNote")]
    show_first_run_note: bool,
    state: String,
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// True when the decrypted JSON carries the `liveActivity` key (even as `null`) or
/// `notificationsAuthorized`, so routing never lets one fall through to a normal registration.
pub fn is_live_activity_payload(payload: &TokenPayload, raw_json: &Value) -> bool {
    let _ = payload;
    raw_json.as_object().is_some_and(|o| {
        o.contains_key("liveActivity") || o.contains_key("notificationsAuthorized")
    })
}

/// Validate a raw `liveActivity` object against the signer of the 3079 event.
pub fn parse(
    raw: &Value,
    signer_hex: &str,
    now_unix: u64,
) -> Result<LiveActivityRegistration, PayloadError> {
    let wire: Wire = serde_json::from_value(raw.clone()).map_err(|e| {
        // serde's own text embeds the offending value, which may be a token.
        PayloadError::Malformed(format!(
            "{:?} at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;

    let token = &wire.activity_push_token;
    if !(32..=512).contains(&token.len()) || !is_lower_hex(token) {
        return Err(PayloadError::BadActivityToken);
    }

    let mut parts = wire.post_coordinate.splitn(3, ':');
    let (kind, author, d_tag) = (parts.next(), parts.next(), parts.next());
    let (Some(kind), Some(author), Some(d_tag)) = (kind, author, d_tag) else {
        return Err(PayloadError::BadCoordinate(wire.post_coordinate));
    };
    let well_formed = !kind.is_empty()
        && kind.bytes().all(|b| b.is_ascii_digit())
        && author.len() == 64
        && is_lower_hex(author)
        && !d_tag.is_empty();
    if !well_formed {
        return Err(PayloadError::BadCoordinate(wire.post_coordinate));
    }
    if author != signer_hex {
        return Err(PayloadError::CoordinateAuthorMismatch {
            coordinate_author: author.to_string(),
            signer: signer_hex.to_string(),
        });
    }

    let n = wire.post_event_ids.len();
    if n == 0 || n > MAX_EVENT_IDS {
        return Err(PayloadError::BadEventIds(format!("got {n} entries")));
    }
    if let Some(bad) = wire
        .post_event_ids
        .iter()
        .find(|id| id.len() != 64 || !is_lower_hex(id))
    {
        return Err(PayloadError::BadEventIds(format!("invalid id {bad}")));
    }

    match wire.window_ends_at.checked_sub(wire.posted_at) {
        Some(w) if w > 0 && w <= MAX_WINDOW_SECS => {}
        _ => return Err(PayloadError::BadWindow),
    }
    // The app derives `windowEndsAt` from the phone's clock, and the event
    // reaches us later, so allow the same future skew as control events.
    let latest_window_end = now_unix
        .saturating_add(MAX_WINDOW_SECS)
        .saturating_add(crate::event_handler::CONTROL_EVENT_FUTURE_SKEW_SECS);
    if wire.window_ends_at > latest_window_end {
        return Err(PayloadError::WindowTooFarAhead);
    }

    let state = match wire.state.as_str() {
        "active" => ActivityState::Active,
        "ended" => ActivityState::Ended,
        _ => return Err(PayloadError::BadState),
    };

    Ok(LiveActivityRegistration {
        activity_token: wire.activity_push_token,
        post_coordinate: wire.post_coordinate,
        post_event_ids: wire.post_event_ids,
        posted_at: wire.posted_at,
        window_ends_at: wire.window_ends_at,
        show_first_run_note: wire.show_first_run_note,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::TokenPayload;
    use serde_json::{json, Value};

    const SIGNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NOW: u64 = 1_800_000_000;

    fn good() -> Value {
        json!({
            "activityPushToken": "ab".repeat(32),
            "postCoordinate": format!("34236:{SIGNER}:my-post"),
            "postEventIds": ["b".repeat(64), "c".repeat(64)],
            "postedAt": NOW - 60,
            "windowEndsAt": NOW - 60 + 3600,
            "showFirstRunNote": true,
            "state": "active"
        })
    }

    fn with(key: &str, v: Value) -> Value {
        let mut g = good();
        g[key] = v;
        g
    }

    #[test]
    fn parses_app_wire_shape() {
        let raw: Value = serde_json::from_str(&format!(
            r#"{{"activityPushToken":"{}","postCoordinate":"34236:{SIGNER}:my-post","postEventIds":["{}"],"postedAt":1799999940,"windowEndsAt":1800003540,"showFirstRunNote":false,"state":"ended"}}"#,
            "0f".repeat(40),
            "d".repeat(64)
        ))
        .unwrap();
        let r = parse(&raw, SIGNER, NOW).unwrap();
        assert_eq!(r.activity_token, "0f".repeat(40));
        assert_eq!(r.post_coordinate, format!("34236:{SIGNER}:my-post"));
        assert_eq!(r.post_event_ids, vec!["d".repeat(64)]);
        assert_eq!(r.posted_at, 1_799_999_940);
        assert_eq!(r.window_ends_at, 1_800_003_540);
        assert!(!r.show_first_run_note);
        assert_eq!(r.state, ActivityState::Ended);
    }

    #[test]
    fn token_payload_keeps_unknown_live_activity_raw() {
        let p: TokenPayload = serde_json::from_value(json!({
            "token": "fcm",
            "notificationsAuthorized": false,
            "liveActivity": {"anything": [1, 2]}
        }))
        .unwrap();
        assert_eq!(p.notifications_authorized, Some(false));
        assert_eq!(p.live_activity, Some(json!({"anything": [1, 2]})));
    }

    #[test]
    fn rejects_bad_token_hex() {
        for t in [
            "XY".repeat(20),
            "ab".repeat(8),
            "ab".repeat(300),
            "AB".repeat(20),
        ] {
            let e = parse(&with("activityPushToken", json!(t)), SIGNER, NOW).unwrap_err();
            assert!(matches!(e, PayloadError::BadActivityToken), "{t}");
        }
    }

    #[test]
    fn error_text_never_contains_the_token() {
        let t = "zz".repeat(20);
        let e = parse(&with("activityPushToken", json!(t)), SIGNER, NOW).unwrap_err();
        assert!(!e.to_string().contains(&t));
    }

    #[test]
    fn rejects_bad_coordinate() {
        for c in [
            "nope",
            "x:aaaa:d",
            &format!("34236:{SIGNER}:"),
            &format!("34236:{}:d", "A".repeat(64)),
        ] {
            let e = parse(&with("postCoordinate", json!(c)), SIGNER, NOW).unwrap_err();
            assert!(matches!(e, PayloadError::BadCoordinate(_)), "{c}");
        }
    }

    #[test]
    fn error_text_clips_long_client_values() {
        let long_d = "é".repeat(500);
        let coordinate = format!("34236:{}:{long_d}", "A".repeat(64));
        let e = parse(&with("postCoordinate", json!(coordinate)), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadCoordinate(_)));
        let text = e.to_string();
        assert!(!text.contains(&long_d), "{text}");
        assert!(text.ends_with('…'), "{text}");
        let echoed = text.split_once(": ").unwrap().1;
        assert_eq!(echoed.chars().count(), MAX_ECHOED_CHARS + 1);

        let bad_id = "ü".repeat(300);
        let e = parse(&with("postEventIds", json!([bad_id])), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadEventIds(_)));
        let text = e.to_string();
        assert!(!text.contains(&bad_id), "{text}");
        assert!(text.ends_with('…'), "{text}");

        assert_eq!(clip("short"), "short");
        assert_eq!(
            clip(&"x".repeat(MAX_ECHOED_CHARS)),
            "x".repeat(MAX_ECHOED_CHARS)
        );
    }

    #[test]
    fn rejects_coordinate_of_another_author() {
        let other = "e".repeat(64);
        let e = parse(
            &with("postCoordinate", json!(format!("34236:{other}:d"))),
            SIGNER,
            NOW,
        )
        .unwrap_err();
        assert!(matches!(e, PayloadError::CoordinateAuthorMismatch { .. }));
    }

    #[test]
    fn rejects_empty_event_ids() {
        let e = parse(&with("postEventIds", json!([])), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadEventIds(_)));
    }

    #[test]
    fn rejects_eleven_event_ids() {
        let ids: Vec<String> = (0..11).map(|_| "b".repeat(64)).collect();
        let e = parse(&with("postEventIds", json!(ids)), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadEventIds(_)));
        let e = parse(&with("postEventIds", json!(["short"])), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadEventIds(_)));
    }

    #[test]
    fn rejects_window_over_an_hour() {
        let e = parse(&with("windowEndsAt", json!(NOW - 60 + 3601)), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadWindow));
        let e = parse(&with("windowEndsAt", json!(NOW - 60)), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadWindow));
        let e = parse(&with("windowEndsAt", json!(NOW - 100)), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadWindow));
    }

    #[test]
    fn rejects_window_far_future() {
        let mut g = good();
        g["postedAt"] = json!(NOW + 3000);
        g["windowEndsAt"] = json!(NOW + 3000 + 3600);
        let e = parse(&g, SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::WindowTooFarAhead));
    }

    /// The app computes `windowEndsAt` from the phone's clock, so a phone a
    /// second ahead of the service must still be accepted.
    #[test]
    fn accepts_window_from_a_clock_slightly_ahead() {
        let mut g = good();
        g["postedAt"] = json!(NOW + 1);
        g["windowEndsAt"] = json!(NOW + 1 + 3600);
        let r = parse(&g, SIGNER, NOW).unwrap();
        assert_eq!(r.window_ends_at, NOW + 3601);
    }

    #[test]
    fn rejects_window_beyond_the_skew_allowance() {
        let mut g = good();
        g["postedAt"] = json!(NOW + 301);
        g["windowEndsAt"] = json!(NOW + 301 + 3600);
        let e = parse(&g, SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::WindowTooFarAhead));
        g["postedAt"] = json!(NOW + 300);
        g["windowEndsAt"] = json!(NOW + 300 + 3600);
        assert!(parse(&g, SIGNER, NOW).is_ok());
    }

    #[test]
    fn rejects_unknown_state() {
        let e = parse(&with("state", json!("paused")), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::BadState));
    }

    #[test]
    fn malformed_text_never_echoes_client_values() {
        let secret = "ab".repeat(32);
        let cases = [
            with("activityPushToken", json!(123_456_789)),
            with("postEventIds", json!(secret.clone())),
            with("postedAt", json!(secret.clone())),
            with("showFirstRunNote", json!(secret.clone())),
        ];
        for raw in cases {
            let e = parse(&raw, SIGNER, NOW).unwrap_err();
            assert!(matches!(e, PayloadError::Malformed(_)));
            let text = e.to_string();
            assert!(!text.contains(&secret), "{text}");
            assert!(!text.contains("123456789"), "{text}");
        }
    }

    #[test]
    fn rejects_missing_or_mistyped_fields() {
        let e = parse(&json!({}), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::Malformed(_)));
        let e = parse(&with("showFirstRunNote", json!("yes")), SIGNER, NOW).unwrap_err();
        assert!(matches!(e, PayloadError::Malformed(_)));
    }

    #[test]
    fn null_live_activity_is_still_a_live_activity_payload() {
        let raw = json!({"token": "fcm", "liveActivity": null});
        let p: TokenPayload = serde_json::from_value(raw.clone()).unwrap();
        assert!(p.live_activity.is_none());
        assert!(is_live_activity_payload(&p, &raw));
        let raw = json!({"token": "fcm", "notificationsAuthorized": true});
        let p: TokenPayload = serde_json::from_value(raw.clone()).unwrap();
        assert!(is_live_activity_payload(&p, &raw));
        let raw = json!({"token": "fcm"});
        let p: TokenPayload = serde_json::from_value(raw.clone()).unwrap();
        assert!(!is_live_activity_payload(&p, &raw));
    }
}
