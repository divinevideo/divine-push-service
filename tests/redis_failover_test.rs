use bb8_redis::bb8::Pool;
use divine_push_service::redis_connection::PrimaryConnectionManager;
use std::time::Duration;

/// Demotes and re-promotes the Redis server, so it needs a dedicated instance:
/// set `REDIS_FAILOVER_TEST_URL` (for example `redis://127.0.0.1:6390`) to run it.
#[tokio::test]
async fn pooled_connection_to_demoted_primary_is_not_handed_out() {
    let Ok(url) = std::env::var("REDIS_FAILOVER_TEST_URL") else {
        println!("Skipping test: REDIS_FAILOVER_TEST_URL not set");
        return;
    };

    let admin = redis::Client::open(url.as_str()).unwrap();
    let mut admin = admin.get_multiplexed_async_connection().await.unwrap();
    let pool = Pool::builder()
        .max_size(1)
        .connection_timeout(Duration::from_secs(1))
        .build(PrimaryConnectionManager::new(url.as_str()).unwrap())
        .await
        .unwrap();

    {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("SET")
            .arg("failover_test:key")
            .arg("before")
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    // Demote the node the pooled connection points at. Port 1 never answers,
    // so the node stays a replica until it is promoted again below.
    redis::cmd("REPLICAOF")
        .arg("127.0.0.1")
        .arg(1)
        .query_async::<()>(&mut admin)
        .await
        .unwrap();

    let while_replica = pool.get().await.map(|_| ());

    redis::cmd("REPLICAOF")
        .arg("NO")
        .arg("ONE")
        .query_async::<()>(&mut admin)
        .await
        .unwrap();

    assert!(
        while_replica.is_err(),
        "checkout handed out a connection to a replica"
    );

    let mut conn = pool.get().await.unwrap();
    redis::cmd("SET")
        .arg("failover_test:key")
        .arg("after")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
}
