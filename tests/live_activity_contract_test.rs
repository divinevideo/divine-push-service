//! Shared Live Activity contract fixture, copied into divine-mobile.
//!
//! Each repo pins the same sha256, which catches an accidental edit in one repo
//! but not drift between them. Changing the contract takes two coordinated
//! PRs, one per repo, each updating the file and its pin.

use serde::Deserialize;
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/live_activity_contract.json");
const CONTRACT_SHA256: &str = "4e8fd159739a6dcfec5c5e37794c5a34768b7350ef101b41ba3fc0f632a449d0";

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
    expected: Expected,
}

#[derive(Deserialize)]
struct Expected {
    action: String,
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
