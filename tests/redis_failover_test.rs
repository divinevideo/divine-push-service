//! These tests demote and re-promote the Redis server, so they need a dedicated
//! instance: set `REDIS_FAILOVER_TEST_URL` (for example `redis://127.0.0.1:6390`)
//! to run them.

use bb8_redis::bb8::{Pool, RunError};
use divine_push_service::redis_connection::PrimaryConnectionManager;
use redis::aio::MultiplexedConnection;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;

/// Every test here reconfigures the same server, so they must not overlap.
static SERVER: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const ROLELESS_USER: &str = "failover_test_no_role";

fn failover_url() -> Option<String> {
    let url = std::env::var("REDIS_FAILOVER_TEST_URL").ok();
    if url.is_none() {
        println!("Skipping test: REDIS_FAILOVER_TEST_URL not set");
    }
    url
}

async fn admin(url: &str) -> MultiplexedConnection {
    redis::Client::open(url)
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
}

async fn pool(url: &str) -> Pool<PrimaryConnectionManager> {
    Pool::builder()
        .max_size(1)
        .connection_timeout(Duration::from_secs(1))
        .build(PrimaryConnectionManager::new(url).unwrap())
        .await
        .unwrap()
}

/// Port 1 never answers, so the node stays a replica until `promote`.
async fn demote(admin: &mut MultiplexedConnection) {
    redis::cmd("REPLICAOF")
        .arg("127.0.0.1")
        .arg(1)
        .query_async::<()>(admin)
        .await
        .unwrap();
}

async fn promote(admin: &mut MultiplexedConnection) {
    redis::cmd("REPLICAOF")
        .arg("NO")
        .arg("ONE")
        .query_async::<()>(admin)
        .await
        .unwrap();
}

async fn set(conn: &mut impl redis::aio::ConnectionLike) -> redis::RedisResult<()> {
    redis::cmd("SET")
        .arg("failover_test:key")
        .arg("value")
        .query_async(conn)
        .await
}

/// Returns a failed checkout instead of panicking, so the caller can restore
/// the server before it asserts.
async fn checkout_and_set(
    pool: &Pool<PrimaryConnectionManager>,
) -> Result<redis::RedisResult<()>, RunError<redis::RedisError>> {
    let mut conn = pool.get().await?;
    Ok(set(&mut *conn).await)
}

#[tokio::test]
async fn pooled_connection_to_demoted_primary_is_not_handed_out() {
    let Some(url) = failover_url() else { return };
    let _server = SERVER.lock().await;
    let mut admin = admin(&url).await;
    let pool = pool(&url).await;

    set(&mut *pool.get().await.unwrap()).await.unwrap();

    demote(&mut admin).await;
    let while_replica = pool.get().await.map(|_| ());
    promote(&mut admin).await;

    assert!(
        while_replica.is_err(),
        "checkout handed out a connection to a replica"
    );
    set(&mut *pool.get().await.unwrap()).await.unwrap();
}

/// With `ROLE` refused, checkout falls back to `PING`, which a replica passes.
/// The `READONLY` reply itself must still get the connection dropped.
#[tokio::test]
async fn connection_that_hit_readonly_is_dropped_when_role_is_refused() {
    let Some(url) = failover_url() else { return };
    let _server = SERVER.lock().await;
    let mut admin = admin(&url).await;
    redis::cmd("ACL")
        .arg(&[
            "SETUSER",
            ROLELESS_USER,
            "reset",
            "on",
            "nopass",
            "~*",
            "+@all",
            "-role",
        ])
        .query_async::<()>(&mut admin)
        .await
        .unwrap();
    let mut roleless_url = url::Url::parse(&url).unwrap();
    roleless_url.set_username(ROLELESS_USER).unwrap();
    roleless_url.set_password(Some("unused")).unwrap();
    let pool = pool(roleless_url.as_str()).await;

    set(&mut *pool.get().await.unwrap()).await.unwrap();

    demote(&mut admin).await;
    let write_while_replica = checkout_and_set(&pool).await;
    let pooled_after_readonly = pool.state().connections;
    promote(&mut admin).await;
    let after_promotion = checkout_and_set(&pool).await;
    redis::cmd("ACL")
        .arg(&["DELUSER", ROLELESS_USER])
        .query_async::<()>(&mut admin)
        .await
        .unwrap();

    assert_eq!(
        write_while_replica
            .expect("checkout failed with ROLE refused")
            .unwrap_err()
            .kind(),
        redis::ErrorKind::ReadOnly
    );
    assert_eq!(
        pooled_after_readonly, 0,
        "connection that hit READONLY went back into the pool"
    );
    after_promotion
        .expect("checkout failed after promotion")
        .unwrap();
}

async fn connections_received(admin: &mut MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("stats")
        .query_async(admin)
        .await
        .unwrap();
    info.lines()
        .find_map(|line| line.strip_prefix("total_connections_received:"))
        .and_then(|n| n.trim().parse().ok())
        .unwrap()
}

/// A replica must be rejected when the connection is opened, not only at
/// checkout, so reconnects wait on the pool's backoff. Rejecting only at
/// checkout reconnects in a tight loop for the whole connection timeout.
#[tokio::test]
async fn replica_is_rejected_on_connect_without_a_reconnect_storm() {
    let Some(url) = failover_url() else { return };
    let _server = SERVER.lock().await;
    let mut admin = admin(&url).await;
    let pool = pool(&url).await;

    demote(&mut admin).await;
    let before = connections_received(&mut admin).await;
    let while_replica = pool.get().await.map(|_| ());
    let opened = connections_received(&mut admin).await - before;
    promote(&mut admin).await;

    assert!(
        while_replica.is_err(),
        "checkout handed out a connection to a replica"
    );
    assert!(
        opened < 100,
        "one checkout opened {opened} connections to the replica"
    );
    set(&mut *pool.get().await.unwrap()).await.unwrap();
}
