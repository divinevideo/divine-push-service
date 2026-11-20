//! Shared Live Activity contract fixture, copied into divine-mobile.
//!
//! Each repo pins the same sha256, which catches an accidental edit in one repo
//! but not drift between them. Changing the contract takes two coordinated
//! PRs, one per repo, each updating the file and its pin.

use divine_push_service::live_activity::content::{
    decide, needs_views_fallback, ContentState, Decision,
};
use divine_push_service::live_activity::payload::{ActivityState, LiveActivityRegistration};
use divine_push_service::live_activity::stats_client::{BulkStats, VersionStats};
use divine_push_service::live_activity::store::StoredActivity;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/live_activity_contract.json");
const CONTRACT_SHA256: &str = "52c18d40f29b4a24458e1fd33a211b86ec1bab96d74f30d1f3de57e072089f55";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    version: u32,
    timings: Timings,
    counts: Vec<CountCase>,
    content_states: Vec<ContentStateCase>,
    links: Vec<LinkCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Timings {
    settle_seconds: u64,
    window_seconds: u64,
    cadence_seconds: u64,
    stale_seconds: u64,
    summary_dismissal_seconds: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountCase {
    name: String,
    seconds_since_posted: u64,
    response: String,
    versions: Vec<serde_json::Value>,
    views_fallback: serde_json::Value,
    absent_checks_before: u32,
    expect_views_lookup: Option<bool>,
    expected: Expected,
}

#[derive(Deserialize)]
struct Expected {
    action: String,
    phase: Option<String>,
    views: Option<u64>,
    likes: Option<u64>,
    comments: Option<u64>,
}

#[derive(Deserialize)]
struct ContentStateCase {
    name: String,
    json: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkCase {
    post_event_id: String,
    post_coordinate: String,
    url: String,
}

#[test]
fn fixture_checksum_is_pinned() {
    let digest = hex::encode(Sha256::digest(FIXTURE.as_bytes()));
    assert_eq!(
        digest, CONTRACT_SHA256,
        "the contract fixture changed: update both repos' copies and pins together"
    );
}

#[test]
fn fixture_parses() {
    let f: Fixture = serde_json::from_str(FIXTURE).expect("fixture must deserialize");
    assert_eq!(f.version, 1);
    assert_eq!(f.timings.settle_seconds, 600);
    assert_eq!(f.timings.window_seconds, 3600);
    assert_eq!(f.timings.cadence_seconds, 180);
    assert_eq!(f.timings.stale_seconds, 600);
    assert_eq!(f.timings.summary_dismissal_seconds, 1800);

    assert!(f.counts.len() >= 13);
    for case in &f.counts {
        assert!(
            ["update", "end", "skip"].contains(&case.expected.action.as_str()),
            "{}",
            case.name
        );
    }
    let no_lookup: Vec<&str> = f
        .counts
        .iter()
        .filter(|c| c.expect_views_lookup == Some(false))
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(no_lookup, ["missing_post_gets_no_fallback"]);
    assert!(f.content_states.len() >= 2);
    assert!(f.content_states.iter().all(|c| !c.name.is_empty()));
    assert!(f
        .content_states
        .iter()
        .any(|c| c.json.get("postCoordinate").is_none()));
    assert!(f.links.len() >= 3);
    for link in &f.links {
        assert_eq!(link.post_event_id.len(), 64);
        assert!(link.url.contains(&link.post_event_id));
        assert!(link.post_coordinate.starts_with("34236:"));
    }
}

fn event_id(i: usize) -> String {
    format!("{:064x}", i + 1)
}

#[test]
fn count_cases() {
    let f: Fixture = serde_json::from_str(FIXTURE).unwrap();
    let now = 1_800_000_000u64;
    for case in &f.counts {
        let posted_at = now - case.seconds_since_posted;
        let ids: Vec<String> = (0..case.versions.len().max(1)).map(event_id).collect();
        let activity = StoredActivity {
            id: "a".into(),
            owner: "b".repeat(64),
            fcm_token: "fcm".into(),
            registration: LiveActivityRegistration {
                activity_token: "ab".into(),
                post_coordinate: format!("34236:{}:d", "c".repeat(64)),
                post_event_ids: ids.clone(),
                posted_at,
                window_ends_at: posted_at + f.timings.window_seconds,
                show_first_run_note: false,
                state: ActivityState::Active,
            },
            first_seen_at: posted_at,
            absent_checks: 0,
            last_content: None,
        };
        let mut bulk = BulkStats::default();
        for (i, v) in case.versions.iter().enumerate() {
            if v.as_str() == Some("missing") {
                bulk.missing.insert(ids[i].clone());
            } else {
                let views = v["views"].as_u64();
                bulk.stats.insert(
                    ids[i].clone(),
                    VersionStats {
                        views,
                        reactions: v["reactions"].as_u64().unwrap(),
                        comments: v["comments"].as_u64().unwrap(),
                    },
                );
            }
        }
        let stats = if case.response == "failed" {
            Err(())
        } else {
            Ok(&bulk)
        };
        let fallback = match &case.views_fallback {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::String(_) => Err(()),
            v => Ok(Some(v.as_u64().unwrap())),
        };
        if case.expect_views_lookup == Some(false) {
            assert!(!needs_views_fallback(&activity, &bulk), "{}", case.name);
        }
        let (decision, _) = decide(&activity, stats, fallback, case.absent_checks_before, now);
        let (action, content) = match &decision {
            Decision::Update(c) => ("update", Some(c)),
            Decision::End(c) => ("end", Some(c)),
            Decision::Skip => ("skip", None),
        };
        assert_eq!(action, case.expected.action, "{}", case.name);
        if let Some(c) = content {
            let phase = serde_json::to_value(c.phase).unwrap();
            assert_eq!(
                phase.as_str(),
                case.expected.phase.as_deref(),
                "{}",
                case.name
            );
            assert_eq!(c.views, case.expected.views, "{} views", case.name);
            assert_eq!(c.likes, case.expected.likes, "{} likes", case.name);
            assert_eq!(c.comments, case.expected.comments, "{} comments", case.name);
        }
    }
}

/// Every content state the server could send (one carrying `postCoordinate`)
/// survives a round trip through the server's own type unchanged, so the keys
/// and value shapes the server writes are exactly the ones the app reads.
#[test]
fn content_states_round_trip_through_the_server_type() {
    let f: Fixture = serde_json::from_str(FIXTURE).unwrap();
    let mut checked = 0;
    for case in &f.content_states {
        if case.json.get("postCoordinate").is_none() {
            continue;
        }
        let state: ContentState = serde_json::from_value(case.json.clone())
            .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        let back = serde_json::to_value(&state).unwrap();
        assert_eq!(back, case.json, "{}", case.name);
        checked += 1;
    }
    assert!(
        checked > 0,
        "no content state with a postCoordinate was checked"
    );
}
