use divine_push_service::event_handler;
use nostr_sdk::prelude::*;
use std::time::Duration;

#[test]
fn test_replay_horizon_ignores_old_events() {
    // Create an event that's 8 days old (beyond 7-day horizon)
    let old_timestamp = Timestamp::now() - Duration::from_secs(8 * 24 * 60 * 60);

    let event = EventBuilder::new(Kind::TextNote, "Old message")
        .custom_created_at(old_timestamp)
        .sign_with_keys(&Keys::generate())
        .unwrap();

    // The event should be rejected due to age
    assert!(event_handler::is_event_too_old(&event));
}

#[test]
fn test_replay_horizon_accepts_recent_events() {
    // Create an event that's 1 day old (within 7-day horizon)
    let recent_timestamp = Timestamp::now() - Duration::from_secs(24 * 60 * 60);

    let event = EventBuilder::new(Kind::TextNote, "Recent message")
        .custom_created_at(recent_timestamp)
        .sign_with_keys(&Keys::generate())
        .unwrap();

    // The event should be accepted
    assert!(!event_handler::is_event_too_old(&event));
}

#[test]
fn test_replay_horizon_edge_case() {
    // Straddle the boundary rather than sitting on it. This test and
    // `is_event_too_old` each call `Timestamp::now()`, and the horizon only
    // moves forward between the two, so an event built at *exactly* 7 days
    // flips to "too old" whenever a second ticks over in between. A minute of
    // slack on each side pins the same cutoff without the coin flip.
    const HORIZON_SECS: u64 = 7 * 24 * 60 * 60;
    const SLACK_SECS: u64 = 60;

    let build = |age_secs: u64| {
        EventBuilder::new(Kind::TextNote, "Boundary message")
            .custom_created_at(Timestamp::now() - Duration::from_secs(age_secs))
            .sign_with_keys(&Keys::generate())
            .unwrap()
    };

    // 7 days is the cutoff, not 6.
    assert!(!event_handler::is_event_too_old(&build(
        HORIZON_SECS - SLACK_SECS
    )));
    // And it is a cutoff, not a floor.
    assert!(event_handler::is_event_too_old(&build(
        HORIZON_SECS + SLACK_SECS
    )));
}

#[test]
fn test_replay_horizon_future_events() {
    // Create an event with a future timestamp (shouldn't happen but let's handle it)
    let future_timestamp = Timestamp::now() + Duration::from_secs(60 * 60); // 1 hour in future

    let event = EventBuilder::new(Kind::TextNote, "Future message")
        .custom_created_at(future_timestamp)
        .sign_with_keys(&Keys::generate())
        .unwrap();

    // Future events should be accepted (they're not old)
    assert!(!event_handler::is_event_too_old(&event));
}

/// Build a kind-30000 list event with the given `d` tag, aged by `age`.
fn aged_list_event(d_tag: &str, age: Duration) -> Event {
    EventBuilder::new(Kind::from(30000u16), "")
        .tag(Tag::identifier(d_tag))
        .tag(Tag::public_key(Keys::generate().public_key()))
        .custom_created_at(Timestamp::now() - age)
        .sign_with_keys(&Keys::generate())
        .unwrap()
}

#[test]
fn test_notify_list_survives_the_replay_horizon() {
    // The whole feature rests on this. A bell list is replaceable state, not a
    // timely trigger: one published 90 days ago and never touched since is
    // still the user's current subscription set. If the horizon aged it out,
    // every user who set their bells more than a week ago would silently get
    // nothing, with no error anywhere.
    let list = aged_list_event("notify", Duration::from_secs(90 * 24 * 60 * 60));

    assert!(
        event_handler::is_event_too_old(&list),
        "the event really is beyond the horizon"
    );
    assert!(
        !event_handler::is_beyond_replay_horizon(&list),
        "but the handler loop must not drop it"
    );
}

#[test]
fn test_recent_notify_list_is_kept() {
    let list = aged_list_event("notify", Duration::from_secs(60 * 60));

    assert!(!event_handler::is_beyond_replay_horizon(&list));
}

#[test]
fn test_old_content_events_are_still_dropped() {
    let old_video = EventBuilder::new(Kind::from(34236u16), "video")
        .tag(Tag::identifier("vid-1"))
        .custom_created_at(Timestamp::now() - Duration::from_secs(8 * 24 * 60 * 60))
        .sign_with_keys(&Keys::generate())
        .unwrap();

    assert!(event_handler::is_beyond_replay_horizon(&old_video));
}

#[test]
fn test_exemption_does_not_cover_other_kind_30000_lists() {
    // The relay filter narrows to `#d=notify`, but a buggy or hostile relay can
    // send any kind 30000. An unrelated people list is not subscription state
    // this service owns, so it gets no exemption from the horizon.
    let other = aged_list_event("mute", Duration::from_secs(90 * 24 * 60 * 60));

    assert!(event_handler::is_beyond_replay_horizon(&other));
}
