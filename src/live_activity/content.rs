//! Content rules for Live Activity updates.
//!
//! Pure functions that decide what each update shows: counts (with the settle
//! rule), the "can't find" check, the window-end summary, and what to freeze on
//! when stats stay unreachable. Network and budget handling live in the worker.

use serde::{Deserialize, Serialize};

use crate::live_activity::stats_client::BulkStats;
use crate::live_activity::store::StoredActivity;

/// Seconds after the settle anchor before a 0 count is trusted.
pub const SETTLE_SECONDS: u64 = 600;

/// Which face of the Live Activity the app shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Posting,
    Live,
    NotFound,
    Summary,
}

/// The `content-state` object sent to the app. Keys are a shared contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentState {
    pub phase: Phase,
    /// `None` means unknown.
    pub views: Option<u64>,
    pub likes: Option<u64>,
    pub comments: Option<u64>,
    /// Unix seconds when these numbers were last true.
    pub updated_at: u64,
    pub show_first_run_note: bool,
    pub post_coordinate: String,
    pub post_event_id: String,
}

/// What the worker should do with an activity this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Send an `update` event.
    Update(ContentState),
    /// Send an `end` event and finish the activity.
    End(ContentState),
    /// Send nothing; try again next cycle.
    Skip,
}

fn settle_at(activity: &StoredActivity) -> u64 {
    activity.registration.posted_at.max(activity.first_seen_at) + SETTLE_SECONDS
}

fn state(
    activity: &StoredActivity,
    phase: Phase,
    counts: (Option<u64>, Option<u64>, Option<u64>),
    now_unix: u64,
) -> ContentState {
    let reg = &activity.registration;
    ContentState {
        phase,
        views: counts.0,
        likes: counts.1,
        comments: counts.2,
        updated_at: now_unix,
        show_first_run_note: reg.show_first_run_note,
        post_coordinate: reg.post_coordinate.clone(),
        post_event_id: reg.post_event_ids.first().cloned().unwrap_or_default(),
    }
}

/// True only when at least one of the post's ids is present and every present
/// id reports null views, so the `/views` fallback applies. Never true for a
/// post with no present ids: `/views` answers 0 for unknown posts.
pub fn needs_views_fallback(activity: &StoredActivity, stats: &BulkStats) -> bool {
    let mut present = activity
        .registration
        .post_event_ids
        .iter()
        .filter_map(|id| stats.stats.get(id))
        .peekable();
    present.peek().is_some() && present.all(|s| s.views.is_none())
}

/// Decides what this cycle shows. Returns the decision and the new
/// `absent_checks` count to store.
///
/// `views_fallback` is only consulted when [`needs_views_fallback`] is true. A
/// failed request or failed fallback is a skip and leaves the count unchanged.
pub fn decide(
    activity: &StoredActivity,
    stats: Result<&BulkStats, ()>,
    views_fallback: Result<Option<u64>, ()>,
    absent_checks_before: u32,
    now_unix: u64,
) -> (Decision, u32) {
    let Ok(stats) = stats else {
        return (Decision::Skip, absent_checks_before);
    };
    let reg = &activity.registration;
    let settled = now_unix >= settle_at(activity);
    let present: Vec<_> = reg
        .post_event_ids
        .iter()
        .filter_map(|id| stats.stats.get(id))
        .collect();

    if present.is_empty() {
        // Before settle, absence is expected lag and does not count.
        let after = if settled {
            absent_checks_before.saturating_add(1)
        } else {
            absent_checks_before
        };
        let nulls = (None, None, None);
        if now_unix >= reg.window_ends_at {
            return (
                Decision::End(state(activity, Phase::Summary, nulls, now_unix)),
                after,
            );
        }
        if settled && after >= 2 {
            return (
                Decision::End(state(activity, Phase::NotFound, nulls, now_unix)),
                after,
            );
        }
        return (
            Decision::Update(state(activity, Phase::Live, nulls, now_unix)),
            after,
        );
    }

    let mut views = present.iter().filter_map(|s| s.views).max();
    if needs_views_fallback(activity, stats) {
        match views_fallback {
            Ok(v) => views = v,
            Err(()) => return (Decision::Skip, absent_checks_before),
        }
    }
    let likes = present.iter().map(|s| s.reactions).max();
    let comments = present.iter().map(|s| s.comments).max();
    let settle = |v: Option<u64>| if settled { v } else { v.filter(|n| *n != 0) };
    let counts = (settle(views), settle(likes), settle(comments));

    if now_unix >= reg.window_ends_at {
        return (
            Decision::End(state(activity, Phase::Summary, counts, now_unix)),
            0,
        );
    }
    (
        Decision::Update(state(activity, Phase::Live, counts, now_unix)),
        0,
    )
}

/// The content to end on when stats stay unreachable until the window closes:
/// the last content sent, unchanged, or `live` with null counts when nothing was
/// ever sent. `updated_at` stays as last sent so it says when numbers were true.
pub fn outage_end(
    activity: &StoredActivity,
    last_content: Option<&serde_json::Value>,
    now_unix: u64,
) -> ContentState {
    last_content
        .and_then(|v| serde_json::from_value::<ContentState>(v.clone()).ok())
        .unwrap_or_else(|| state(activity, Phase::Live, (None, None, None), now_unix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_state_serializes_exact_keys() {
        let c = ContentState {
            phase: Phase::NotFound,
            views: Some(3),
            likes: None,
            comments: Some(0),
            updated_at: 1_800_000_000,
            show_first_run_note: true,
            post_coordinate: "34236:aa:d".into(),
            post_event_id: "bb".into(),
        };
        let v = serde_json::to_value(&c).unwrap();
        let o = v.as_object().unwrap();
        let mut keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "comments",
                "likes",
                "phase",
                "postCoordinate",
                "postEventId",
                "showFirstRunNote",
                "updatedAt",
                "views"
            ]
        );
        assert_eq!(o["phase"], "notFound");
        assert!(o["views"].is_u64());
        assert!(o["likes"].is_null());
        assert!(o["comments"].is_u64());
        assert!(o["updatedAt"].is_u64());
        assert!(o["showFirstRunNote"].is_boolean());
        assert!(o["postCoordinate"].is_string());
        assert!(o["postEventId"].is_string());
    }

    use crate::live_activity::payload::{ActivityState, LiveActivityRegistration};
    use crate::live_activity::stats_client::VersionStats;

    const NOW: u64 = 1_800_000_000;

    fn activity(posted_at: u64, first_seen_at: u64) -> StoredActivity {
        StoredActivity {
            id: "a".into(),
            owner: "b".repeat(64),
            fcm_token: "fcm".into(),
            registration: LiveActivityRegistration {
                activity_token: "ab".into(),
                post_coordinate: "34236:cc:d".into(),
                post_event_ids: vec!["e1".into()],
                posted_at,
                window_ends_at: posted_at + 3600,
                show_first_run_note: true,
                state: ActivityState::Active,
            },
            first_seen_at,
            absent_checks: 0,
            last_content: None,
        }
    }

    fn zero_stats() -> BulkStats {
        let mut b = BulkStats::default();
        b.stats.insert(
            "e1".into(),
            VersionStats {
                views: Some(0),
                reactions: 0,
                comments: 0,
            },
        );
        b
    }

    #[test]
    fn late_first_seen_delays_settle() {
        // Posted 12 minutes ago but only first seen 2 minutes ago: not settled.
        let a = activity(NOW - 720, NOW - 120);
        let (d, _) = decide(&a, Ok(&zero_stats()), Ok(None), 0, NOW);
        let Decision::Update(c) = d else {
            panic!("expected update")
        };
        assert_eq!((c.views, c.likes, c.comments), (None, None, None));
        // Once 600s past first_seen_at, the zeros are confirmed.
        let (d, _) = decide(&a, Ok(&zero_stats()), Ok(None), 0, NOW - 120 + 600);
        let Decision::Update(c) = d else {
            panic!("expected update")
        };
        assert_eq!((c.views, c.likes, c.comments), (Some(0), Some(0), Some(0)));
    }

    #[test]
    fn counter_semantics_for_absent_checks() {
        let a = activity(NOW - 900, NOW - 900);
        let absent = BulkStats::default();
        assert_eq!(decide(&a, Ok(&absent), Ok(None), 1, NOW).1, 2);
        assert_eq!(decide(&a, Ok(&zero_stats()), Ok(None), 1, NOW).1, 0);
        assert_eq!(decide(&a, Err(()), Ok(None), 1, NOW), (Decision::Skip, 1));
        let early = activity(NOW - 100, NOW - 100);
        assert_eq!(decide(&early, Ok(&absent), Ok(None), 1, NOW).1, 1);
    }

    #[test]
    fn failed_request_at_window_end_skips() {
        let a = activity(NOW - 3600, NOW - 3600);
        assert_eq!(decide(&a, Err(()), Ok(None), 0, NOW).0, Decision::Skip);
    }

    #[test]
    fn outage_end_returns_last_content_unchanged() {
        let a = activity(NOW - 3600, NOW - 3600);
        let last = ContentState {
            phase: Phase::Live,
            views: Some(7),
            likes: Some(1),
            comments: None,
            updated_at: NOW - 500,
            show_first_run_note: false,
            post_coordinate: "34236:cc:d".into(),
            post_event_id: "e1".into(),
        };
        let v = serde_json::to_value(&last).unwrap();
        assert_eq!(outage_end(&a, Some(&v), NOW), last);
    }

    #[test]
    fn outage_end_falls_back_when_nothing_sent_or_unparseable() {
        let a = activity(NOW - 3600, NOW - 3600);
        for last in [None, Some(serde_json::json!({"bogus": 1}))] {
            let c = outage_end(&a, last.as_ref(), NOW);
            assert_eq!(c.phase, Phase::Live);
            assert_eq!((c.views, c.likes, c.comments), (None, None, None));
            assert_eq!(c.updated_at, NOW);
            assert_eq!(c.post_event_id, "e1");
            assert!(c.show_first_run_note);
        }
    }
}
