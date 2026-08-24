// ABOUTME: Covers the notify-list exemption from the handler loop's event claim.
// ABOUTME: Redis-backed cases skip cleanly when Redis is unavailable, per the dedup_test convention.

use divine_push_service::{event_handler, redis_store};
use nostr_sdk::prelude::*;
use std::time::Duration;

async fn create_test_pool() -> Option<redis_store::RedisPool> {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    match redis_store::create_pool(&redis_url, 5).await {
        Ok(pool) => {
            let mut conn = match pool.get().await {
                Ok(conn) => conn,
                Err(_) => {
                    println!("Skipping test: Redis not available");
                    return None;
                }
            };

            let ping_result: redis::RedisResult<String> =
                redis::cmd("PING").query_async(&mut *conn).await;

            if ping_result.is_err() {
                println!("Skipping test: Redis not available");
                return None;
            }

            drop(conn);
            Some(pool)
        }
        Err(_) => {
            println!("Skipping test: Redis not available");
            None
        }
    }
}

/// Build a kind-30000 list event with the given `d` tag.
fn list_event(d_tag: &str) -> Event {
    EventBuilder::new(Kind::from(30000u16), "")
        .tag(Tag::identifier(d_tag))
        .tag(Tag::public_key(Keys::generate().public_key()))
        .sign_with_keys(&Keys::generate())
        .unwrap()
}

#[test]
fn test_notify_lists_are_exempt_from_the_event_claim() {
    // The claim is taken before routing and never released. A notify list whose
    // handler hits a transient Redis error therefore leaves a claim standing for
    // an event that was never applied, and the historical replay on the next
    // restart skips it — the subscriber's bells stay dark for the full dedup
    // TTL. The claim buys nothing in exchange: the Lua script behind
    // `replace_notify_subscriptions` is atomic and rejects anything not strictly
    // newer, so duplicate delivery is already a no-op.
    assert!(!event_handler::requires_event_claim(&list_event("notify")));
}

#[test]
fn test_content_events_use_recipient_claims_instead_of_event_claims() {
    let video = EventBuilder::new(Kind::from(34236u16), "video")
        .tag(Tag::identifier("vid-1"))
        .sign_with_keys(&Keys::generate())
        .unwrap();
    let reaction = EventBuilder::new(Kind::from(7u16), "+")
        .sign_with_keys(&Keys::generate())
        .unwrap();
    let note = EventBuilder::new(Kind::TextNote, "hello")
        .sign_with_keys(&Keys::generate())
        .unwrap();

    assert!(!event_handler::requires_event_claim(&video));
    assert!(!event_handler::requires_event_claim(&reaction));
    assert!(!event_handler::requires_event_claim(&note));
    assert!(!event_handler::requires_event_claim(&list_event("mute")));
}

#[test]
fn test_control_events_keep_the_event_claim() {
    for kind in [3079u16, 3080, 3083] {
        let event = EventBuilder::new(Kind::from(kind), "ciphertext")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert!(event_handler::requires_event_claim(&event));
    }
}

#[tokio::test]
async fn test_a_standing_claim_would_have_skipped_the_replayed_list() {
    // Pins the mechanism the exemption bypasses, so the two tests above are not
    // just asserting a negation of themselves. A claim taken on the first
    // attempt survives the handler's failure, and the replay's attempt to claim
    // the same event id comes back false — which, without the exemption, is the
    // `continue` that drops the list.
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let list = list_event("notify");

    let first = redis_store::try_claim_event(&pool, &list.id, 60)
        .await
        .expect("first claim");
    let replay = redis_store::try_claim_event(&pool, &list.id, 60)
        .await
        .expect("replayed claim");

    let mut conn = pool.get().await.expect("redis connection");
    let _: redis::RedisResult<i64> = redis::cmd("DEL")
        .arg(format!("dedup:{}", list.id.to_hex()))
        .query_async(&mut *conn)
        .await;

    assert!(first, "the failed first attempt still took the claim");
    assert!(
        !replay,
        "so the replay is refused, and only the exemption lets the list through"
    );
}

#[tokio::test]
async fn test_a_replayed_list_still_applies_without_a_claim() {
    // The other half: with the claim out of the way, re-applying the same list
    // is safe. `replace_notify_subscriptions` treats the duplicate as an
    // idempotent repair and leaves the index intact, which is why the claim was
    // never load-bearing here.
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let created_at = Timestamp::now().as_secs();
    // A replay is the same event arriving twice, so both calls carry the same
    // id — which is also what makes the guard's tie-break reject the second.
    let event_id = list_event("notify").id;

    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        std::slice::from_ref(&creator),
        created_at,
        &event_id,
    )
    .await
    .expect("first apply");

    let reapplied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        std::slice::from_ref(&creator),
        created_at,
        &event_id,
    )
    .await
    .expect("replayed apply");

    let watchers = redis_store::get_notify_watchers(&pool, &creator)
        .await
        .expect("watchers");

    let mut conn = pool.get().await.expect("redis connection");
    for key in [
        format!("notify_subs:{}", subscriber.to_hex()),
        format!("notify_subs_ts:{}", subscriber.to_hex()),
        format!("notify_watchers:{}", creator.to_hex()),
    ] {
        let _: redis::RedisResult<i64> = redis::cmd("DEL").arg(key).query_async(&mut *conn).await;
    }

    assert!(applied, "the list applies the first time");
    assert!(reapplied, "the duplicate is safe to replay for repair");
    assert_eq!(
        watchers,
        vec![subscriber],
        "and the index is unchanged by the replay"
    );
}

#[test]
fn test_the_exemption_composes_with_the_replay_horizon() {
    // Both gates sit in the same loop and both have to let an old list through.
    // Exempting one without the other still loses every bell set more than the
    // horizon ago.
    let old_list = EventBuilder::new(Kind::from(30000u16), "")
        .tag(Tag::identifier("notify"))
        .tag(Tag::public_key(Keys::generate().public_key()))
        .custom_created_at(Timestamp::now() - Duration::from_secs(90 * 24 * 60 * 60))
        .sign_with_keys(&Keys::generate())
        .unwrap();

    assert!(!event_handler::is_beyond_replay_horizon(&old_list));
    assert!(!event_handler::requires_event_claim(&old_list));
}
