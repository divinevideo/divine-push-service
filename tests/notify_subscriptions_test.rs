// ABOUTME: Redis round-trip tests for the notify-list reverse index.
// ABOUTME: Skips cleanly when Redis is unavailable, per the dedup_test convention.

use divine_push_service::redis_store;
use nostr_sdk::prelude::*;

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

/// Remove every key this test family touches for the given pubkeys.
async fn cleanup(pool: &redis_store::RedisPool, keys: &[String]) {
    let mut conn = pool.get().await.expect("redis connection");
    for key in keys {
        let _: redis::RedisResult<i64> = redis::cmd("DEL").arg(key).query_async(&mut *conn).await;
    }
}

/// Deterministic stand-in event id. Only its ordering matters to the guard, so
/// tests that care about a tie build ids explicitly instead of using this.
fn test_event_id(seed: u64) -> EventId {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&seed.to_be_bytes());
    EventId::from_slice(&bytes).expect("32 bytes is a valid event id")
}

fn keys_for(subscriber: &PublicKey, creators: &[PublicKey]) -> Vec<String> {
    let mut keys = vec![
        format!("notify_subs:{}", subscriber.to_hex()),
        format!("notify_subs_ts:{}", subscriber.to_hex()),
    ];
    for creator in creators {
        keys.push(format!("notify_watchers:{}", creator.to_hex()));
    }
    keys
}

#[tokio::test]
async fn test_publishing_a_list_populates_the_reverse_index() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator_a = Keys::generate().public_key();
    let creator_b = Keys::generate().public_key();
    let all = [creator_a, creator_b];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator_a, creator_b],
        1000,
        &test_event_id(1),
    )
    .await
    .expect("replace should succeed");
    assert!(applied, "a first list must be applied");

    let watchers_a = redis_store::get_notify_watchers(&pool, &creator_a)
        .await
        .expect("watchers lookup");
    let watchers_b = redis_store::get_notify_watchers(&pool, &creator_b)
        .await
        .expect("watchers lookup");

    assert_eq!(watchers_a, vec![subscriber]);
    assert_eq!(watchers_b, vec![subscriber]);

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_shrinking_a_list_removes_only_the_dropped_creator() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let kept = Keys::generate().public_key();
    let dropped = Keys::generate().public_key();
    let all = [kept, dropped];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[kept, dropped],
        1000,
        &test_event_id(1000),
    )
    .await
    .expect("initial replace");

    // Republished list without `dropped` — an unbell.
    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[kept],
        2000,
        &test_event_id(2000),
    )
    .await
    .expect("second replace");
    assert!(applied);

    assert_eq!(
        redis_store::get_notify_watchers(&pool, &kept)
            .await
            .expect("watchers lookup"),
        vec![subscriber],
        "a creator still on the list keeps the watcher"
    );
    assert!(
        redis_store::get_notify_watchers(&pool, &dropped)
            .await
            .expect("watchers lookup")
            .is_empty(),
        "an unbelled creator loses the watcher"
    );

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_empty_list_clears_every_subscription() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let all = [creator];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        1000,
        &test_event_id(1000),
    )
    .await
    .expect("initial replace");

    // A user who unbelled everyone publishes a list with no p tags. This is
    // legitimate, not malformed, and must clear the index.
    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[],
        2000,
        &test_event_id(2000),
    )
    .await
    .expect("empty replace");
    assert!(applied);

    assert!(
        redis_store::get_notify_watchers(&pool, &creator)
            .await
            .expect("watchers lookup")
            .is_empty(),
        "an empty list clears the reverse index"
    );

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_older_replacement_is_ignored() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let all = [creator];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        2000,
        &test_event_id(2000),
    )
    .await
    .expect("initial replace");

    // A relay delivering a stale replacement after a newer one must not
    // resurrect an unbell.
    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[],
        1000,
        &test_event_id(1000),
    )
    .await
    .expect("stale replace");
    assert!(!applied, "an older created_at must be rejected");

    assert_eq!(
        redis_store::get_notify_watchers(&pool, &creator)
            .await
            .expect("watchers lookup"),
        vec![subscriber],
        "the newer list survives a stale replacement"
    );

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_replaying_the_same_created_at_is_ignored() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let all = [creator];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        2000,
        &test_event_id(2000),
    )
    .await
    .expect("initial replace");

    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[],
        2000,
        &test_event_id(2000),
    )
    .await
    .expect("equal-timestamp replace");
    assert!(
        !applied,
        "the same event delivered twice is a replay, not an update"
    );

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_a_created_at_tie_is_resolved_by_the_lower_event_id() {
    // NIP-01: "in case of replaceable events with the same timestamp, the event
    // with the lowest id (first in lexical order) should be retained". Resolving
    // by arrival order instead lets this service and the relay hold permanently
    // different lists, so a rebuild from relay history disagrees with what was
    // served live.
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let all = [creator];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    // The higher id arrives first, so it is what we are holding.
    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        2000,
        &test_event_id(9),
    )
    .await
    .expect("initial replace");

    // Same second, lower id: the relay retains this one, so we must too, even
    // though it arrived second.
    let applied =
        redis_store::replace_notify_subscriptions(&pool, &subscriber, &[], 2000, &test_event_id(4))
            .await
            .expect("lower-id replace");
    assert!(
        applied,
        "a same-second event with a lower id is the one NIP-01 retains"
    );
    assert!(
        redis_store::get_notify_watchers(&pool, &creator)
            .await
            .expect("watchers lookup")
            .is_empty(),
        "the retained list is the one that was applied"
    );

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_a_created_at_tie_rejects_a_higher_event_id() {
    // The other side of the tie-break, and the one that reads backwards from
    // "newer wins": arriving second is not enough, the id has to sort lower.
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let all = [creator];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        2000,
        &test_event_id(4),
    )
    .await
    .expect("initial replace");

    let applied =
        redis_store::replace_notify_subscriptions(&pool, &subscriber, &[], 2000, &test_event_id(9))
            .await
            .expect("higher-id replace");
    assert!(
        !applied,
        "a same-second event with a higher id is the one NIP-01 discards"
    );
    assert_eq!(
        redis_store::get_notify_watchers(&pool, &creator)
            .await
            .expect("watchers lookup"),
        vec![subscriber],
        "the lower-id list survives"
    );

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[tokio::test]
async fn test_a_legacy_bare_timestamp_still_guards_and_is_upgraded() {
    // `notify_subs_ts` used to hold a bare integer. Deployed instances have
    // those, so the read path has to keep working: a tie against a value with no
    // id stays rejected (nothing to compare), a newer list still applies, and
    // applying it writes the new format so the tie-break works from then on.
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    let all = [creator];
    cleanup(&pool, &keys_for(&subscriber, &all)).await;

    let ts_key = format!("notify_subs_ts:{}", subscriber.to_hex());
    let mut conn = pool.get().await.expect("redis connection");
    let _: () = redis::cmd("SET")
        .arg(&ts_key)
        .arg(2000)
        .query_async(&mut *conn)
        .await
        .expect("seed the legacy format");

    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        2000,
        &test_event_id(1),
    )
    .await
    .expect("tie against legacy");
    assert!(
        !applied,
        "a tie against a stored value with no id has nothing to break on"
    );

    let applied = redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber,
        &[creator],
        2001,
        &test_event_id(9),
    )
    .await
    .expect("newer than legacy");
    assert!(applied, "a strictly newer list still applies");

    let stored: String = redis::cmd("GET")
        .arg(&ts_key)
        .query_async(&mut *conn)
        .await
        .expect("read back");
    assert_eq!(
        stored,
        format!("2001:{}", test_event_id(9).to_hex()),
        "applying upgrades the key to the tie-breakable format"
    );

    // And the tie-break now works against the upgraded value.
    let applied =
        redis_store::replace_notify_subscriptions(&pool, &subscriber, &[], 2001, &test_event_id(2))
            .await
            .expect("tie against upgraded");
    assert!(applied, "a lower id wins once the stored id is known");

    cleanup(&pool, &keys_for(&subscriber, &all)).await;
}

#[test]
fn test_rate_limit_key_carries_full_pubkeys() {
    let subscriber = Keys::generate().public_key();
    let creator = Keys::generate().public_key();

    let key = redis_store::build_notify_rate_key(&subscriber, &creator);

    // Nostr identifiers are never truncated, in keys or anywhere else.
    assert!(key.contains(&subscriber.to_hex()));
    assert!(key.contains(&creator.to_hex()));
    assert_eq!(
        key,
        format!("notify_rate:{}:{}", subscriber.to_hex(), creator.to_hex())
    );
}

#[tokio::test]
async fn test_rate_limit_window_is_scoped_per_creator() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber = Keys::generate().public_key();
    let throttled = Keys::generate().public_key();
    let other = Keys::generate().public_key();

    let throttled_key = redis_store::build_notify_rate_key(&subscriber, &throttled);
    let other_key = redis_store::build_notify_rate_key(&subscriber, &other);
    cleanup(&pool, &[throttled_key.clone(), other_key.clone()]).await;

    // Simulate a delivered push opening the window for one creator.
    redis_store::set_cached_string(&pool, &throttled_key, "1", 3600)
        .await
        .expect("open window");

    assert!(
        redis_store::get_cached_string(&pool, &throttled_key)
            .await
            .expect("lookup")
            .is_some(),
        "a second video from the same creator is inside the window"
    );
    assert!(
        redis_store::get_cached_string(&pool, &other_key)
            .await
            .expect("lookup")
            .is_none(),
        "a video from a different creator is unaffected"
    );

    cleanup(&pool, &[throttled_key, other_key]).await;
}

#[tokio::test]
async fn test_two_subscribers_watching_one_creator() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let subscriber_a = Keys::generate().public_key();
    let subscriber_b = Keys::generate().public_key();
    let creator = Keys::generate().public_key();
    cleanup(&pool, &keys_for(&subscriber_a, &[creator])).await;
    cleanup(&pool, &keys_for(&subscriber_b, &[creator])).await;

    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber_a,
        &[creator],
        1000,
        &test_event_id(1000),
    )
    .await
    .expect("replace a");
    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber_b,
        &[creator],
        1000,
        &test_event_id(1000),
    )
    .await
    .expect("replace b");

    let mut watchers = redis_store::get_notify_watchers(&pool, &creator)
        .await
        .expect("watchers lookup");
    watchers.sort_by_key(|pk| pk.to_hex());
    let mut expected = vec![subscriber_a, subscriber_b];
    expected.sort_by_key(|pk| pk.to_hex());
    assert_eq!(watchers, expected);

    // One unbelling must not disturb the other.
    redis_store::replace_notify_subscriptions(
        &pool,
        &subscriber_a,
        &[],
        2000,
        &test_event_id(2000),
    )
    .await
    .expect("unbell a");
    assert_eq!(
        redis_store::get_notify_watchers(&pool, &creator)
            .await
            .expect("watchers lookup"),
        vec![subscriber_b]
    );

    cleanup(&pool, &keys_for(&subscriber_a, &[creator])).await;
    cleanup(&pool, &keys_for(&subscriber_b, &[creator])).await;
}
