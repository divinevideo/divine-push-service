//! Staleness-score refresh for tokens a push was actually delivered to.
//!
//! The cleanup sweep deletes by the `stale_tokens` score, and until now that
//! score was written only at registration. These cover the contract that makes
//! the sweep mean "inactive" instead of "registered long ago".

use divine_push_service::redis_store;
use nostr_sdk::{Keys, Timestamp};

/// Documented in AGENTS.md; the constant itself is private to `redis_store`.
const STALE_TOKENS_ZSET: &str = "stale_tokens";

const DAY_SECS: u64 = 24 * 60 * 60;

async fn test_pool() -> Option<redis_store::RedisPool> {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let pool = redis_store::create_pool(&redis_url, 5).await.ok()?;
    let mut conn = pool.get().await.ok()?;
    let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
    drop(conn);
    pong.ok().map(|_| pool)
}

async fn score_of(pool: &redis_store::RedisPool, token: &str) -> Option<u64> {
    let mut conn = pool.get().await.unwrap();
    let score: Option<f64> = redis::cmd("ZSCORE")
        .arg(STALE_TOKENS_ZSET)
        .arg(token)
        .query_async(&mut *conn)
        .await
        .unwrap();
    score.map(|s| s as u64)
}

async fn backdate(pool: &redis_store::RedisPool, token: &str, score: u64) {
    let mut conn = pool.get().await.unwrap();
    redis::cmd("ZADD")
        .arg(STALE_TOKENS_ZSET)
        .arg(score)
        .arg(token)
        .query_async::<i64>(&mut *conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_refresh_moves_a_registered_tokens_score_forward() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let owner = Keys::generate();
    let token = format!("refresh-forward-{}", owner.public_key().to_hex());

    redis_store::add_or_update_token(&pool, &owner.public_key(), &token)
        .await
        .unwrap();

    // A device registered 80 days ago and never re-registered: 10 days from
    // deletion under the 90-day sweep.
    let registered_at = Timestamp::now().as_secs() - 80 * DAY_SECS;
    backdate(&pool, &token, registered_at).await;

    let refreshed = redis_store::refresh_token_activity(&pool, std::slice::from_ref(&token))
        .await
        .unwrap();

    assert_eq!(
        refreshed, 1,
        "the one live token should have been refreshed"
    );
    let score = score_of(&pool, &token).await.expect("token still tracked");
    assert!(
        score > registered_at,
        "a delivered push must move the token away from the sweep, but the score stayed at {score}"
    );
    assert!(
        Timestamp::now().as_secs() - score < 60,
        "the refreshed score should be roughly now, got {score}"
    );

    redis_store::remove_token(&pool, &owner.public_key(), &token)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_refresh_never_resurrects_a_token_that_is_no_longer_registered() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let owner = Keys::generate();
    let token = format!("refresh-absent-{}", owner.public_key().to_hex());

    // Deregistered — or swept — between the send and the refresh. Re-adding it
    // would leave a tracked token with no owner and no user-set membership,
    // which the sweep can only clear after another full max-age window.
    redis_store::add_or_update_token(&pool, &owner.public_key(), &token)
        .await
        .unwrap();
    redis_store::remove_token(&pool, &owner.public_key(), &token)
        .await
        .unwrap();

    let refreshed = redis_store::refresh_token_activity(&pool, std::slice::from_ref(&token))
        .await
        .unwrap();

    assert_eq!(refreshed, 0, "a token nobody tracks must not be refreshed");
    assert!(
        score_of(&pool, &token).await.is_none(),
        "the refresh re-created a deregistered token in the staleness set"
    );
}

#[tokio::test]
async fn a_refresh_never_moves_a_score_backwards() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let owner = Keys::generate();
    let token = format!("refresh-backwards-{}", owner.public_key().to_hex());

    redis_store::add_or_update_token(&pool, &owner.public_key(), &token)
        .await
        .unwrap();

    // A replica with a fast clock stamped the score ahead of this one's `now`.
    // Taking the lower value would pull the token closer to the sweep, which is
    // the opposite of what recording activity is for.
    let ahead = Timestamp::now().as_secs() + 10 * DAY_SECS;
    backdate(&pool, &token, ahead).await;

    redis_store::refresh_token_activity(&pool, std::slice::from_ref(&token))
        .await
        .unwrap();

    assert_eq!(
        score_of(&pool, &token).await,
        Some(ahead),
        "a refresh must never lower a token's score"
    );

    redis_store::remove_token(&pool, &owner.public_key(), &token)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_refresh_of_no_tokens_touches_redis_not_at_all() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };

    let refreshed = redis_store::refresh_token_activity(&pool, &[])
        .await
        .unwrap();

    assert_eq!(refreshed, 0, "an empty refresh is a no-op, not an error");
}
