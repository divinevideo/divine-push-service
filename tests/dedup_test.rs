use divine_push_service::redis_store;
use futures::future::join_all;
use nostr_sdk::prelude::*;
use tokio::time::{sleep, Duration};

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

fn generate_event_id() -> EventId {
    EventBuilder::new(Kind::TextNote, "dedup integration test")
        .sign_with_keys(&Keys::generate())
        .unwrap()
        .id
}

#[tokio::test]
async fn test_dedup_single_event_claimed_once() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let event_id = generate_event_id();

    let first_claim = redis_store::try_claim_event(&pool, &event_id, 30).await.unwrap();
    assert!(first_claim, "first claim should succeed");

    let second_claim = redis_store::try_claim_event(&pool, &event_id, 30).await.unwrap();
    assert!(!second_claim, "second claim should be rejected");
}

#[tokio::test]
async fn test_dedup_ttl_expiry_allows_reprocessing() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let event_id = generate_event_id();

    let first_claim = redis_store::try_claim_event(&pool, &event_id, 1).await.unwrap();
    assert!(first_claim, "initial claim should succeed");

    sleep(Duration::from_secs(2)).await;

    let second_claim = redis_store::try_claim_event(&pool, &event_id, 1).await.unwrap();
    assert!(second_claim, "claim should succeed again after TTL expiration");
}

#[tokio::test]
async fn test_dedup_concurrent_claims_only_one_wins() {
    let Some(pool) = create_test_pool().await else {
        return;
    };

    let event_id = generate_event_id();
    let task_count = 10;

    let tasks = (0..task_count).map(|_| {
        let pool = pool.clone();
        let event_id = event_id;
        tokio::spawn(async move { redis_store::try_claim_event(&pool, &event_id, 30).await })
    });

    let outcomes = join_all(tasks).await;

    let mut success_count = 0;
    for outcome in outcomes {
        let claimed = outcome.unwrap().unwrap();
        if claimed {
            success_count += 1;
        }
    }

    assert_eq!(
        success_count, 1,
        "exactly one concurrent claimant should succeed"
    );
}
