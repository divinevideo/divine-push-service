//! Bounded `SCAN` discovery of every registered pubkey, for the roster publisher.
//!
//! `all_registered_pubkeys` walks the whole `user_tokens:*` keyspace, so these
//! cover that it finds every owner and that one corrupt key cannot fail the run.

use divine_push_service::redis_store;
use nostr_sdk::Keys;

async fn test_pool() -> Option<redis_store::RedisPool> {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let pool = redis_store::create_pool(&redis_url, 5).await.ok()?;
    let mut conn = pool.get().await.ok()?;
    let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
    drop(conn);
    pong.ok().map(|_| pool)
}

#[tokio::test]
async fn all_registered_pubkeys_finds_every_registered_owner() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let first = Keys::generate();
    let second = Keys::generate();
    let first_token = format!("scan-roster-first-{}", first.public_key().to_hex());
    let second_token = format!("scan-roster-second-{}", second.public_key().to_hex());

    redis_store::add_or_update_token(&pool, &first.public_key(), &first_token)
        .await
        .unwrap();
    redis_store::add_or_update_token(&pool, &second.public_key(), &second_token)
        .await
        .unwrap();

    let pubkeys = redis_store::all_registered_pubkeys(&pool).await.unwrap();

    assert!(pubkeys.contains(&first.public_key().to_hex()));
    assert!(pubkeys.contains(&second.public_key().to_hex()));

    redis_store::remove_token(&pool, &first.public_key(), &first_token)
        .await
        .unwrap();
    redis_store::remove_token(&pool, &second.public_key(), &second_token)
        .await
        .unwrap();
}

#[tokio::test]
async fn all_registered_pubkeys_skips_a_malformed_key() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let malformed_key = "user_tokens:not-a-pubkey";
    let mut conn = pool.get().await.unwrap();
    redis::cmd("SADD")
        .arg(malformed_key)
        .arg("some-token")
        .query_async::<i64>(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let pubkeys = redis_store::all_registered_pubkeys(&pool).await.unwrap();

    assert!(
        !pubkeys.iter().any(|pubkey| pubkey == "not-a-pubkey"),
        "a malformed user_tokens key must be skipped, not returned"
    );

    let mut conn = pool.get().await.unwrap();
    redis::cmd("DEL")
        .arg(malformed_key)
        .query_async::<i64>(&mut *conn)
        .await
        .unwrap();
}
