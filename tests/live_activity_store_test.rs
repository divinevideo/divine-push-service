//! Redis store for Live Activities: registration, ordering, tombstones, caps and
//! the due schedule, kept apart from device registration (`user_tokens:*`).
//!
//! Every test uses its own owner pubkey and tokens, so tests can run in parallel
//! against one Redis. Tests that call `claim_due` (a global operation) take
//! `CLAIM_LOCK` and assert only on their own ids; every other test schedules its
//! jobs far in the future (`FAR_DUE_MS`) so a claim never reaches them.

use divine_push_service::live_activity::payload::{ActivityState, LiveActivityRegistration};
use divine_push_service::live_activity::store::{self, EndOutcome, UpsertOutcome};
use divine_push_service::redis_store::{self, RedisPool};
use nostr_sdk::Keys;
use std::time::{SystemTime, UNIX_EPOCH};

/// Far beyond any `now_ms` a claim test passes, and below 2^53.
const FAR_DUE_MS: u64 = 4_000_000_000_000_000;
const PER_OWNER: usize = 5;
const TOTAL: usize = 100_000;

static CLAIM_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn test_pool() -> Option<RedisPool> {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let pool = redis_store::create_pool(&redis_url, 5).await.ok()?;
    let mut conn = pool.get().await.ok()?;
    let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
    drop(conn);
    pong.ok().map(|_| pool)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn hex(n: usize) -> String {
    let mut s = String::new();
    while s.len() < n {
        s.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    s.truncate(n);
    s
}

fn owner() -> String {
    Keys::generate().public_key().to_hex()
}

fn reg(owner: &str, d: &str, ids: &[&str]) -> LiveActivityRegistration {
    let posted_at = now_secs() - 60;
    LiveActivityRegistration {
        activity_token: hex(64),
        post_coordinate: format!("34236:{owner}:{d}"),
        post_event_ids: ids.iter().map(|s| s.to_string()).collect(),
        posted_at,
        window_ends_at: posted_at + 3600,
        show_first_run_note: true,
        state: ActivityState::Active,
    }
}

/// Activity ids are scoped by owner: blake3 of `{owner}:{activity_token}`. The
/// owner is the coordinate's author, which payload validation pins to the signer.
fn id_of(reg: &LiveActivityRegistration) -> String {
    let owner = reg.post_coordinate.split(':').nth(1).unwrap();
    blake3::hash(format!("{owner}:{}", reg.activity_token).as_bytes())
        .to_hex()
        .to_string()
}

fn ev(c: char) -> String {
    c.to_string().repeat(64)
}

async fn upsert(
    pool: &RedisPool,
    owner: &str,
    fcm: &str,
    r: &LiveActivityRegistration,
    at: u64,
    id: &str,
    due: u64,
) -> UpsertOutcome {
    store::upsert_active(pool, owner, fcm, r, at, id, due, PER_OWNER, TOTAL)
        .await
        .unwrap()
}

async fn hgetall(pool: &RedisPool, key: &str) -> std::collections::HashMap<String, String> {
    let mut conn = pool.get().await.unwrap();
    redis::cmd("HGETALL")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .unwrap()
}

async fn job_score(pool: &RedisPool, id: &str) -> Option<u64> {
    let mut conn = pool.get().await.unwrap();
    redis::cmd("ZSCORE")
        .arg("live_activity_jobs")
        .arg(id)
        .query_async(&mut *conn)
        .await
        .unwrap()
}

async fn ttl(pool: &RedisPool, key: &str) -> i64 {
    let mut conn = pool.get().await.unwrap();
    redis::cmd("TTL")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .unwrap()
}

async fn get(pool: &RedisPool, key: &str) -> Option<String> {
    let mut conn = pool.get().await.unwrap();
    redis::cmd("GET")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .unwrap()
}

async fn smembers(pool: &RedisPool, key: &str) -> Vec<String> {
    let mut conn = pool.get().await.unwrap();
    let mut v: Vec<String> = redis::cmd("SMEMBERS")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    v.sort();
    v
}

fn device_key(owner: &str, fcm: &str) -> String {
    format!(
        "live_activity_device:{owner}:{}",
        blake3::hash(fcm.as_bytes()).to_hex()
    )
}

#[tokio::test]
async fn upsert_creates_and_schedules() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    let before = now_secs();

    let out = upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;
    assert_eq!(out, UpsertOutcome::Created);

    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    assert_eq!(h["owner"], o);
    assert_eq!(h["fcm_token"], fcm);
    assert_eq!(h["activity_token"], r.activity_token);
    assert_eq!(h["post_coordinate"], r.post_coordinate);
    assert_eq!(h["state"], "active");
    assert_eq!(h["last_at"], format!("{:020}", 100));
    assert_eq!(h["last_id"], ev('1'));
    assert_eq!(h["absent_checks"], "0");
    let first_seen: u64 = h["first_seen_at"].parse().unwrap();
    assert!(first_seen >= before && first_seen <= now_secs());
    let ids: Vec<String> = serde_json::from_str(&h["post_event_ids"]).unwrap();
    assert_eq!(ids, vec![ev('a')]);

    assert_eq!(job_score(&pool, &id).await, Some(FAR_DUE_MS));
    assert_eq!(
        smembers(&pool, &format!("live_activity_owner:{o}")).await,
        vec![id.clone()]
    );
    assert_eq!(get(&pool, &device_key(&o, &fcm)).await, Some(id.clone()));

    // Hash lives until window end + 1800 + 300; pointer and owner set share the horizon.
    let horizon = (r.window_ends_at + 2100 - now_secs()) as i64;
    for key in [
        format!("live_activity:{id}"),
        format!("live_activity_owner:{o}"),
        device_key(&o, &fcm),
    ] {
        let t = ttl(&pool, &key).await;
        assert!(
            (horizon - 5..=horizon).contains(&t),
            "{key}: {t} vs {horizon}"
        );
    }
    assert!(store::active_count(&pool).await.unwrap() >= 1);

    store::finish(&pool, &id).await.unwrap();
}

#[tokio::test]
async fn same_token_update_merges_ids_keeps_schedule() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let mut r = reg(&o, "a", &[&ev('a'), &ev('b')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;
    let first_seen = hgetall(&pool, &format!("live_activity:{id}")).await["first_seen_at"].clone();

    r.post_event_ids = vec![ev('c'), ev('a')];
    r.show_first_run_note = false;
    let out = upsert(&pool, &o, &fcm, &r, 101, &ev('2'), FAR_DUE_MS + 99).await;
    assert_eq!(out, UpsertOutcome::Updated);

    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    let ids: Vec<String> = serde_json::from_str(&h["post_event_ids"]).unwrap();
    assert_eq!(ids, vec![ev('a'), ev('b'), ev('c')]);
    assert_eq!(h["last_at"], format!("{:020}", 101));
    assert_eq!(h["first_seen_at"], first_seen);
    assert_eq!(job_score(&pool, &id).await, Some(FAR_DUE_MS));

    // Capped at 10, order kept.
    let many: Vec<String> = ('d'..='p').map(ev).collect();
    r.post_event_ids = many.clone();
    upsert(&pool, &o, &fcm, &r, 102, &ev('3'), FAR_DUE_MS).await;
    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    let ids: Vec<String> = serde_json::from_str(&h["post_event_ids"]).unwrap();
    assert_eq!(ids.len(), 10);
    assert_eq!(&ids[..3], &[ev('a'), ev('b'), ev('c')]);
    assert_eq!(&ids[3..], &many[..7]);

    store::finish(&pool, &id).await.unwrap();
}

#[tokio::test]
async fn new_token_same_device_supersedes() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let old = reg(&o, "first", &[&ev('a')]);
    let old_id = id_of(&old);
    upsert(&pool, &o, &fcm, &old, 100, &ev('1'), FAR_DUE_MS).await;

    let new = reg(&o, "second", &[&ev('b')]);
    let new_id = id_of(&new);
    let out = upsert(&pool, &o, &fcm, &new, 101, &ev('2'), FAR_DUE_MS + 5).await;
    assert_eq!(
        out,
        UpsertOutcome::Superseded {
            previous_id: old_id.clone()
        }
    );

    assert_eq!(job_score(&pool, &old_id).await, None);
    assert!(!store::is_live(&pool, &old_id).await.unwrap());
    let h = hgetall(&pool, &format!("live_activity:{old_id}")).await;
    assert_eq!(h["state"], "ended");
    assert_eq!(job_score(&pool, &new_id).await, Some(FAR_DUE_MS + 5));
    assert_eq!(
        get(&pool, &device_key(&o, &fcm)).await,
        Some(new_id.clone())
    );
    assert_eq!(
        smembers(&pool, &format!("live_activity_owner:{o}")).await,
        vec![new_id.clone()]
    );
    // A different post resets state: nothing is carried.
    let h = hgetall(&pool, &format!("live_activity:{new_id}")).await;
    assert_eq!(h["absent_checks"], "0");

    store::finish(&pool, &new_id).await.unwrap();
}

#[tokio::test]
async fn older_event_is_stale() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let mut r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('5'), FAR_DUE_MS).await;

    r.post_event_ids = vec![ev('z')];
    assert_eq!(
        upsert(&pool, &o, &fcm, &r, 99, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Stale
    );
    // Same second, higher id loses; an exact replay changes nothing either.
    assert_eq!(
        upsert(&pool, &o, &fcm, &r, 100, &ev('9'), FAR_DUE_MS).await,
        UpsertOutcome::Stale
    );
    assert_eq!(
        upsert(&pool, &o, &fcm, &r, 100, &ev('5'), FAR_DUE_MS).await,
        UpsertOutcome::Stale
    );
    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    let ids: Vec<String> = serde_json::from_str(&h["post_event_ids"]).unwrap();
    assert_eq!(ids, vec![ev('a')]);
    // Same second, lower id wins.
    assert_eq!(
        upsert(&pool, &o, &fcm, &r, 100, &ev('2'), FAR_DUE_MS).await,
        UpsertOutcome::Updated
    );

    // A late active for an older activity on the same device does not supersede the newer one.
    let older = reg(&o, "b", &[&ev('b')]);
    assert_eq!(
        upsert(&pool, &o, &fcm, &older, 50, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Stale
    );
    assert!(hgetall(&pool, &format!("live_activity:{}", id_of(&older)))
        .await
        .is_empty());
    assert!(store::is_live(&pool, &id).await.unwrap());

    store::finish(&pool, &id).await.unwrap();
}

#[tokio::test]
async fn end_by_token_orders_and_ends() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let mut r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;

    r.state = ActivityState::Ended;
    let out = store::end_by_token(&pool, &o, &r, 50, &ev('2'))
        .await
        .unwrap();
    assert_eq!(out, EndOutcome::Stale);
    assert!(store::is_live(&pool, &id).await.unwrap());

    let out = store::end_by_token(&pool, &o, &r, 200, &ev('2'))
        .await
        .unwrap();
    assert_eq!(out, EndOutcome::Ended);
    assert!(!store::is_live(&pool, &id).await.unwrap());
    assert_eq!(job_score(&pool, &id).await, None);
    assert!(smembers(&pool, &format!("live_activity_owner:{o}"))
        .await
        .is_empty());
    assert_eq!(get(&pool, &device_key(&o, &fcm)).await, None);
}

#[tokio::test]
async fn another_owner_never_affects_an_activity() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let y = owner();
    let fcm_y = hex(40);
    let r_y = reg(&y, "a", &[&ev('a')]);
    let id_y = id_of(&r_y);
    upsert(&pool, &y, &fcm_y, &r_y, 100, &ev('1'), FAR_DUE_MS).await;

    // Other signers send registrations carrying Y's activity token for their own posts.
    let borrow = |o: &str| {
        let mut r = reg(o, "a", &[&ev('b')]);
        r.activity_token = r_y.activity_token.clone();
        r
    };

    let x_active = owner();
    let r = borrow(&x_active);
    assert_ne!(id_of(&r), id_y);
    let out = upsert(&pool, &x_active, &hex(40), &r, 300, &ev('3'), FAR_DUE_MS).await;
    assert_eq!(
        out,
        UpsertOutcome::TokenOwnedElsewhere,
        "Y's live token is bound to Y"
    );
    assert!(hgetall(&pool, &format!("live_activity:{}", id_of(&r)))
        .await
        .is_empty());

    let x_end = owner();
    let mut r = borrow(&x_end);
    r.state = ActivityState::Ended;
    let out = store::end_by_token(&pool, &x_end, &r, 300, &ev('3'))
        .await
        .unwrap();
    assert_eq!(out, EndOutcome::TombstonedUnseen, "X's own tombstone");
    assert_eq!(
        hgetall(&pool, &format!("live_activity:{}", id_of(&r))).await["owner"],
        x_end
    );

    // Y keeps running and scheduled, untouched.
    assert!(store::is_live(&pool, &id_y).await.unwrap());
    assert_eq!(job_score(&pool, &id_y).await, Some(FAR_DUE_MS));
    let h = hgetall(&pool, &format!("live_activity:{id_y}")).await;
    assert_eq!(h["owner"], y);
    assert_eq!(h["fcm_token"], fcm_y);
    assert_eq!(h["last_at"], format!("{:020}", 100));
    assert_eq!(
        get(&pool, &device_key(&y, &fcm_y)).await,
        Some(id_y.clone())
    );
    assert_eq!(
        smembers(&pool, &format!("live_activity_owner:{y}")).await,
        vec![id_y.clone()]
    );
    // Y can still update and end its own activity.
    assert_eq!(
        upsert(&pool, &y, &fcm_y, &r_y, 400, &ev('4'), FAR_DUE_MS).await,
        UpsertOutcome::Updated
    );
    let mut end_y = r_y.clone();
    end_y.state = ActivityState::Ended;
    assert_eq!(
        store::end_by_token(&pool, &y, &end_y, 500, &ev('5'))
            .await
            .unwrap(),
        EndOutcome::Ended
    );
}

#[tokio::test]
async fn claim_due_skips_a_bad_row_and_keeps_the_batch() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let _guard = CLAIM_LOCK.lock().await;
    let o = owner();
    let good = reg(&o, "good", &[&ev('a')]);
    upsert(&pool, &o, &hex(40), &good, 100, &ev('1'), 7_000).await;

    // A live-looking row that cannot be parsed and whose tombstone write fails
    // (non-numeric window), scheduled ahead of the good one.
    let bad_id = hex(64);
    {
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("HSET")
            .arg(format!("live_activity:{bad_id}"))
            .arg("state")
            .arg("active")
            .arg("owner")
            .arg(&o)
            .arg("window_ends_at")
            .arg("not-a-number")
            .query_async(&mut *conn)
            .await
            .unwrap();
        let _: () = redis::cmd("EXPIRE")
            .arg(format!("live_activity:{bad_id}"))
            .arg(60)
            .query_async(&mut *conn)
            .await
            .unwrap();
        let _: () = redis::cmd("ZADD")
            .arg("live_activity_jobs")
            .arg(6_000)
            .arg(&bad_id)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    let claimed = store::claim_due(&pool, 8_000, 60_000, 1_000).await.unwrap();
    assert!(
        claimed.iter().any(|s| s.id == id_of(&good)),
        "good row kept"
    );
    assert!(claimed.iter().all(|s| s.id != bad_id), "bad row dropped");

    store::finish(&pool, &id_of(&good)).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let _: () = redis::cmd("DEL")
        .arg(format!("live_activity:{bad_id}"))
        .query_async(&mut *conn)
        .await
        .unwrap();
    let _: () = redis::cmd("ZREM")
        .arg("live_activity_jobs")
        .arg(&bad_id)
        .query_async(&mut *conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn replayed_active_after_end_is_tombstoned() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let mut r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;
    r.state = ActivityState::Ended;
    store::end_by_token(&pool, &o, &r, 200, &ev('2'))
        .await
        .unwrap();

    r.state = ActivityState::Active;
    for at in [100, 300] {
        let out = upsert(&pool, &o, &fcm, &r, at, &ev('1'), FAR_DUE_MS).await;
        assert_eq!(out, UpsertOutcome::Tombstoned);
    }
    assert_eq!(job_score(&pool, &id).await, None);
    assert!(!store::is_live(&pool, &id).await.unwrap());
    assert_eq!(get(&pool, &device_key(&o, &fcm)).await, None);
}

#[tokio::test]
async fn claim_due_leases_and_skips_finished() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let _guard = CLAIM_LOCK.lock().await;
    let o = owner();
    let a = reg(&o, "a", &[&ev('a')]);
    let b = reg(&o, "b", &[&ev('b')]);
    let c = reg(&o, "c", &[&ev('c')]);
    let (fa, fb, fc) = (hex(40), hex(40), hex(40));
    upsert(&pool, &o, &fa, &a, 100, &ev('1'), 1_000).await;
    upsert(&pool, &o, &fb, &b, 100, &ev('1'), 1_500).await;
    upsert(&pool, &o, &fc, &c, 100, &ev('1'), 5_000).await;
    store::finish(&pool, &id_of(&b)).await.unwrap();
    // Re-add b's member directly, as a slow worker's leftover would be.
    {
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("ZADD")
            .arg("live_activity_jobs")
            .arg(1_500)
            .arg(id_of(&b))
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    let age = store::oldest_due_age_secs(&pool, 3_000).await.unwrap();
    assert!(age.is_some_and(|a| a >= 2));

    let claimed = store::claim_due(&pool, 2_000, 60_000, 1_000).await.unwrap();
    let mine: Vec<_> = claimed.iter().filter(|s| s.owner == o).collect();
    assert_eq!(mine.len(), 1, "only a is due and live");
    let s = mine[0];
    assert_eq!(s.id, id_of(&a));
    assert_eq!(s.fcm_token, fa);
    assert_eq!(s.registration, a);
    assert_eq!(s.absent_checks, 0);
    assert!(s.last_content.is_none());
    assert!(s.first_seen_at > 0);
    assert_eq!(job_score(&pool, &id_of(&a)).await, Some(62_000));
    assert_eq!(
        job_score(&pool, &id_of(&b)).await,
        None,
        "finished member dropped"
    );
    assert_eq!(job_score(&pool, &id_of(&c)).await, Some(5_000));

    // Leased: a second claim before the lease expires does not return it.
    let again = store::claim_due(&pool, 2_000, 60_000, 1_000).await.unwrap();
    assert!(again.iter().all(|s| s.owner != o));

    store::finish(&pool, &id_of(&a)).await.unwrap();
    store::finish(&pool, &id_of(&c)).await.unwrap();
}

#[tokio::test]
async fn finish_removes_job_and_hash_state() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;

    store::finish(&pool, &id).await.unwrap();
    assert_eq!(job_score(&pool, &id).await, None);
    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    assert_eq!(h["state"], "ended");
    assert!(!store::is_live(&pool, &id).await.unwrap());
    assert!(smembers(&pool, &format!("live_activity_owner:{o}"))
        .await
        .is_empty());
    assert_eq!(get(&pool, &device_key(&o, &fcm)).await, None);
    let t = ttl(&pool, &format!("live_activity:{id}")).await;
    let horizon = (r.window_ends_at + 1800 - now_secs()) as i64;
    assert!((horizon - 5..=horizon).contains(&t), "{t} vs {horizon}");

    // Finishing an unknown id is harmless and creates nothing.
    let ghost = hex(64);
    store::finish(&pool, &ghost).await.unwrap();
    assert!(hgetall(&pool, &format!("live_activity:{ghost}"))
        .await
        .is_empty());
}

#[tokio::test]
async fn never_touches_user_tokens() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let keys = Keys::generate();
    let pk = keys.public_key();
    let o = pk.to_hex();
    let fcm = format!("la-store-user-token-{}", hex(16));
    redis_store::add_or_update_token(&pool, &pk, &fcm)
        .await
        .unwrap();

    async fn snapshot(pool: &RedisPool, o: &str, fcm: &str) -> (Vec<String>, Option<String>) {
        let mut conn = pool.get().await.unwrap();
        let owner: Option<String> = redis::cmd("HGET")
            .arg("token_to_pubkey")
            .arg(fcm)
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        (smembers(pool, &format!("user_tokens:{o}")).await, owner)
    }
    let before = snapshot(&pool, &o, &fcm).await;
    assert_eq!(before, (vec![fcm.clone()], Some(o.clone())));

    let mut a = reg(&o, "a", &[&ev('a')]);
    let b = reg(&o, "b", &[&ev('b')]);
    let id_b = id_of(&b);
    let _guard = CLAIM_LOCK.lock().await;
    upsert(&pool, &o, &fcm, &a, 100, &ev('1'), 10).await;
    assert_eq!(snapshot(&pool, &o, &fcm).await, before);
    upsert(&pool, &o, &fcm, &a, 101, &ev('1'), 10).await;
    assert_eq!(snapshot(&pool, &o, &fcm).await, before);
    upsert(&pool, &o, &fcm, &b, 102, &ev('1'), 10).await;
    assert_eq!(snapshot(&pool, &o, &fcm).await, before);
    store::claim_due(&pool, 20, 1, 1_000).await.unwrap();
    store::record_sent(&pool, &id_b, "{}", 5).await.unwrap();
    store::set_absent_checks(&pool, &id_b, 1).await.unwrap();
    store::reschedule(&pool, &id_b, FAR_DUE_MS).await.unwrap();
    assert_eq!(snapshot(&pool, &o, &fcm).await, before);
    store::finish(&pool, &id_b).await.unwrap();
    a.state = ActivityState::Ended;
    store::end_by_token(&pool, &o, &a, 200, &ev('2'))
        .await
        .unwrap();
    let mut c = reg(&o, "c", &[&ev('c')]);
    c.state = ActivityState::Ended;
    store::end_by_token(&pool, &o, &c, 200, &ev('2'))
        .await
        .unwrap();
    assert_eq!(snapshot(&pool, &o, &fcm).await, before);

    redis_store::remove_token(&pool, &pk, &fcm).await.unwrap();
}

#[tokio::test]
async fn sixth_active_activity_for_owner_is_refused() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let mut held = Vec::new();
    for i in 0..5 {
        let fcm = hex(40);
        let r = reg(&o, &format!("p{i}"), &[&ev('a')]);
        assert_eq!(
            upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await,
            UpsertOutcome::Created
        );
        held.push((fcm, r));
    }

    let fcm6 = hex(40);
    let sixth = reg(&o, "p6", &[&ev('a')]);
    assert_eq!(
        upsert(&pool, &o, &fcm6, &sixth, 100, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::OverOwnerCap
    );
    let id6 = id_of(&sixth);
    assert!(hgetall(&pool, &format!("live_activity:{id6}"))
        .await
        .is_empty());
    assert_eq!(job_score(&pool, &id6).await, None);
    assert_eq!(get(&pool, &device_key(&o, &fcm6)).await, None);
    assert_eq!(
        smembers(&pool, &format!("live_activity_owner:{o}"))
            .await
            .len(),
        5
    );

    // Superseding on a held device, or updating a held activity, is not an extra activity.
    let (fcm0, r0) = &held[0];
    assert_eq!(
        upsert(&pool, &o, fcm0, r0, 101, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Updated
    );
    let replacement = reg(&o, "p0-next", &[&ev('b')]);
    assert_eq!(
        upsert(&pool, &o, fcm0, &replacement, 102, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Superseded {
            previous_id: id_of(r0)
        }
    );

    // Once one finishes, a new one fits again.
    store::finish(&pool, &id_of(&held[1].1)).await.unwrap();
    assert_eq!(
        upsert(&pool, &o, &fcm6, &sixth, 103, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Created
    );

    for id in smembers(&pool, &format!("live_activity_owner:{o}")).await {
        store::finish(&pool, &id).await.unwrap();
    }
}

#[tokio::test]
async fn reschedule_after_finish_does_not_revive() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;

    assert!(store::reschedule(&pool, &id, FAR_DUE_MS + 7).await.unwrap());
    assert_eq!(job_score(&pool, &id).await, Some(FAR_DUE_MS + 7));

    store::finish(&pool, &id).await.unwrap();
    assert!(!store::reschedule(&pool, &id, FAR_DUE_MS + 8).await.unwrap());
    assert_eq!(job_score(&pool, &id).await, None);

    let ghost = hex(64);
    assert!(!store::reschedule(&pool, &ghost, FAR_DUE_MS).await.unwrap());
    assert_eq!(job_score(&pool, &ghost).await, None);
}

#[tokio::test]
async fn record_sent_round_trips_last_content() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let _guard = CLAIM_LOCK.lock().await;
    let o = owner();
    let fcm = hex(40);
    let r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &fcm, &r, 100, &ev('1'), 30_000).await;

    let content = serde_json::json!({"phase": "live", "views": 12, "likes": null});
    store::record_sent(&pool, &id, &content.to_string(), 1_234)
        .await
        .unwrap();
    store::set_absent_checks(&pool, &id, 2).await.unwrap();
    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    assert_eq!(h["last_sent_at"], "1234");

    let claimed = store::claim_due(&pool, 31_000, 1_000, 1_000).await.unwrap();
    let s = claimed.iter().find(|s| s.id == id).expect("claimed");
    assert_eq!(s.last_content, Some(content));
    assert_eq!(s.absent_checks, 2);

    // Writes to a finished or unknown activity never create or alter a hash.
    store::finish(&pool, &id).await.unwrap();
    store::record_sent(&pool, &id, "{\"x\":1}", 9)
        .await
        .unwrap();
    store::set_absent_checks(&pool, &id, 9).await.unwrap();
    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    assert_eq!(h["last_sent_at"], "1234");
    assert_eq!(h["absent_checks"], "2");
    let ghost = hex(64);
    store::record_sent(&pool, &ghost, "{}", 9).await.unwrap();
    store::set_absent_checks(&pool, &ghost, 1).await.unwrap();
    assert!(hgetall(&pool, &format!("live_activity:{ghost}"))
        .await
        .is_empty());
}

#[tokio::test]
async fn ended_before_active_never_starts() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let mut r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    r.state = ActivityState::Ended;
    let out = store::end_by_token(&pool, &o, &r, 200, &ev('2'))
        .await
        .unwrap();
    assert_eq!(out, EndOutcome::TombstonedUnseen);

    let t = ttl(&pool, &format!("live_activity:{id}")).await;
    let horizon = (r.window_ends_at + 1800 - now_secs()) as i64;
    assert!((horizon - 5..=horizon).contains(&t), "{t} vs {horizon}");

    r.state = ActivityState::Active;
    let out = upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;
    assert_eq!(out, UpsertOutcome::Tombstoned);
    assert_eq!(job_score(&pool, &id).await, None);
    assert!(smembers(&pool, &format!("live_activity_owner:{o}"))
        .await
        .is_empty());
    assert_eq!(get(&pool, &device_key(&o, &fcm)).await, None);

    // A replayed end is a no-op.
    r.state = ActivityState::Ended;
    let out = store::end_by_token(&pool, &o, &r, 200, &ev('2'))
        .await
        .unwrap();
    assert_eq!(out, EndOutcome::Stale);
}

#[tokio::test]
async fn update_replaces_fcm_token() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let (old_fcm, new_fcm) = (hex(40), hex(40));
    let r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &old_fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;

    let out = upsert(&pool, &o, &new_fcm, &r, 101, &ev('1'), FAR_DUE_MS).await;
    assert_eq!(out, UpsertOutcome::Updated);
    let h = hgetall(&pool, &format!("live_activity:{id}")).await;
    assert_eq!(h["fcm_token"], new_fcm);
    assert_eq!(
        get(&pool, &device_key(&o, &new_fcm)).await,
        Some(id.clone())
    );
    assert_eq!(get(&pool, &device_key(&o, &old_fcm)).await, None);
    assert_eq!(job_score(&pool, &id).await, Some(FAR_DUE_MS));

    store::finish(&pool, &id).await.unwrap();
}

#[tokio::test]
async fn token_rotation_for_same_post_carries_state() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let old = reg(&o, "same", &[&ev('a')]);
    let old_id = id_of(&old);
    upsert(&pool, &o, &fcm, &old, 100, &ev('1'), FAR_DUE_MS).await;
    let content = serde_json::json!({"phase": "live", "views": 3});
    store::record_sent(&pool, &old_id, &content.to_string(), 77)
        .await
        .unwrap();
    store::set_absent_checks(&pool, &old_id, 1).await.unwrap();
    store::reschedule(&pool, &old_id, FAR_DUE_MS + 42)
        .await
        .unwrap();
    // Backdate first_seen_at so a carry-over is distinguishable from a fresh stamp.
    {
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("HSET")
            .arg(format!("live_activity:{old_id}"))
            .arg("first_seen_at")
            .arg(12_345)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    let mut rotated = reg(&o, "same", &[&ev('a'), &ev('b')]);
    rotated.posted_at = old.posted_at;
    rotated.window_ends_at = old.window_ends_at;
    let new_id = id_of(&rotated);
    let out = upsert(&pool, &o, &fcm, &rotated, 101, &ev('2'), FAR_DUE_MS).await;
    assert_eq!(
        out,
        UpsertOutcome::Superseded {
            previous_id: old_id.clone()
        }
    );

    let h = hgetall(&pool, &format!("live_activity:{new_id}")).await;
    assert_eq!(h["first_seen_at"], "12345");
    assert_eq!(h["absent_checks"], "1");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&h["last_content"]).unwrap(),
        content
    );
    assert_eq!(h["last_sent_at"], "77");
    assert_eq!(h["activity_token"], rotated.activity_token);
    assert_eq!(job_score(&pool, &new_id).await, Some(FAR_DUE_MS + 42));
    assert_eq!(job_score(&pool, &old_id).await, None);
    assert_eq!(
        hgetall(&pool, &format!("live_activity:{old_id}")).await["state"],
        "ended"
    );
    assert_eq!(
        get(&pool, &device_key(&o, &fcm)).await,
        Some(new_id.clone())
    );

    store::finish(&pool, &new_id).await.unwrap();
}

#[tokio::test]
async fn total_cap_refuses_new_activity() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let held = reg(&o, "held", &[&ev('a')]);
    upsert(&pool, &o, &hex(40), &held, 100, &ev('1'), FAR_DUE_MS).await;
    assert!(store::active_count(&pool).await.unwrap() >= 1);

    let fcm = hex(40);
    let r = reg(&o, "new", &[&ev('b')]);
    let out = store::upsert_active(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS, PER_OWNER, 1)
        .await
        .unwrap();
    assert_eq!(out, UpsertOutcome::OverTotalCap);
    let id = id_of(&r);
    assert!(hgetall(&pool, &format!("live_activity:{id}"))
        .await
        .is_empty());
    assert_eq!(job_score(&pool, &id).await, None);
    assert_eq!(get(&pool, &device_key(&o, &fcm)).await, None);

    store::finish(&pool, &id_of(&held)).await.unwrap();
}

#[tokio::test]
async fn tombstone_drops_tokens() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let check = |h: std::collections::HashMap<String, String>| {
        assert_eq!(h["state"], "ended");
        assert_eq!(h["owner"], o);
        assert!(!h.contains_key("fcm_token"), "{h:?}");
        assert!(!h.contains_key("activity_token"), "{h:?}");
    };

    // Finished.
    let a = reg(&o, "a", &[&ev('a')]);
    upsert(&pool, &o, &hex(40), &a, 100, &ev('1'), FAR_DUE_MS).await;
    store::finish(&pool, &id_of(&a)).await.unwrap();
    check(hgetall(&pool, &format!("live_activity:{}", id_of(&a))).await);

    // Ended by the person.
    let mut b = reg(&o, "b", &[&ev('b')]);
    upsert(&pool, &o, &hex(40), &b, 100, &ev('1'), FAR_DUE_MS).await;
    b.state = ActivityState::Ended;
    store::end_by_token(&pool, &o, &b, 200, &ev('2'))
        .await
        .unwrap();
    let h = hgetall(&pool, &format!("live_activity:{}", id_of(&b))).await;
    assert_eq!(h["last_at"], format!("{:020}", 200));
    check(h);

    // Superseded.
    let fcm = hex(40);
    let c = reg(&o, "c", &[&ev('c')]);
    upsert(&pool, &o, &fcm, &c, 100, &ev('1'), FAR_DUE_MS).await;
    let d = reg(&o, "d", &[&ev('d')]);
    upsert(&pool, &o, &fcm, &d, 101, &ev('1'), FAR_DUE_MS).await;
    check(hgetall(&pool, &format!("live_activity:{}", id_of(&c))).await);

    // Unseen.
    let mut e = reg(&o, "e", &[&ev('e')]);
    e.state = ActivityState::Ended;
    store::end_by_token(&pool, &o, &e, 200, &ev('2'))
        .await
        .unwrap();
    let h = hgetall(&pool, &format!("live_activity:{}", id_of(&e))).await;
    assert_eq!(h["last_at"], format!("{:020}", 200));
    assert_eq!(h["last_id"], ev('2'));
    check(h);

    store::finish(&pool, &id_of(&d)).await.unwrap();
}

#[tokio::test]
async fn end_wins_a_same_second_tie() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let mut r = reg(&o, "a", &[&ev('a')]);
    let id = id_of(&r);
    upsert(&pool, &o, &hex(40), &r, 100, &ev('1'), FAR_DUE_MS).await;

    // A higher id in the same second would lose an active-vs-active tie, but an
    // end is terminal, like a deregistration, so the person's end is never lost.
    r.state = ActivityState::Ended;
    let out = store::end_by_token(&pool, &o, &r, 100, &ev('9'))
        .await
        .unwrap();
    assert_eq!(out, EndOutcome::Ended);
    assert!(!store::is_live(&pool, &id).await.unwrap());
}

#[tokio::test]
async fn fcm_change_onto_a_held_device_supersedes_its_activity() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let (fcm_a, fcm_b) = (hex(40), hex(40));
    let a = reg(&o, "a", &[&ev('a')]);
    let b = reg(&o, "b", &[&ev('b')]);
    upsert(&pool, &o, &fcm_a, &a, 100, &ev('1'), FAR_DUE_MS).await;
    upsert(&pool, &o, &fcm_b, &b, 101, &ev('1'), FAR_DUE_MS).await;

    // a moves to fcm_b with a newer stamp: one active activity per device.
    let out = upsert(&pool, &o, &fcm_b, &a, 102, &ev('1'), FAR_DUE_MS).await;
    assert_eq!(
        out,
        UpsertOutcome::Superseded {
            previous_id: id_of(&b)
        }
    );
    assert!(!store::is_live(&pool, &id_of(&b)).await.unwrap());
    assert_eq!(get(&pool, &device_key(&o, &fcm_b)).await, Some(id_of(&a)));
    assert_eq!(get(&pool, &device_key(&o, &fcm_a)).await, None);

    // An older stamp cannot take a device from a newer activity.
    let c = reg(&o, "c", &[&ev('c')]);
    upsert(&pool, &o, &fcm_a, &c, 200, &ev('1'), FAR_DUE_MS).await;
    let out = upsert(&pool, &o, &fcm_a, &a, 150, &ev('1'), FAR_DUE_MS).await;
    assert_eq!(out, UpsertOutcome::Stale);
    assert!(store::is_live(&pool, &id_of(&c)).await.unwrap());

    store::finish(&pool, &id_of(&a)).await.unwrap();
    store::finish(&pool, &id_of(&c)).await.unwrap();
}

#[tokio::test]
async fn active_past_its_dismissal_horizon_is_stale() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let o = owner();
    let fcm = hex(40);
    let held = reg(&o, "held", &[&ev('a')]);
    upsert(&pool, &o, &hex(40), &held, 100, &ev('1'), FAR_DUE_MS).await;

    let mut r = reg(&o, "old", &[&ev('b')]);
    r.window_ends_at = now_secs() - 1800;
    r.posted_at = r.window_ends_at - 3600;
    let out = upsert(&pool, &o, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;
    assert_eq!(out, UpsertOutcome::Stale);
    assert!(hgetall(&pool, &format!("live_activity:{}", id_of(&r)))
        .await
        .is_empty());
    assert_eq!(job_score(&pool, &id_of(&r)).await, None);
    // The owner's other activity is untouched.
    assert_eq!(
        smembers(&pool, &format!("live_activity_owner:{o}")).await,
        vec![id_of(&held)]
    );

    store::finish(&pool, &id_of(&held)).await.unwrap();
}

fn binding_key(activity_token: &str) -> String {
    format!(
        "live_activity_token_owner:{}",
        blake3::hash(activity_token.as_bytes()).to_hex()
    )
}

#[tokio::test]
async fn second_owner_cannot_claim_a_live_token() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let x = owner();
    let fcm_x = hex(40);
    let r_x = reg(&x, "a", &[&ev('a')]);
    let id_x = id_of(&r_x);
    assert_eq!(
        upsert(&pool, &x, &fcm_x, &r_x, 100, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Created
    );
    assert_eq!(
        get(&pool, &binding_key(&r_x.activity_token)).await,
        Some(x.clone())
    );

    let y = owner();
    let fcm_y = hex(40);
    let mut r_y = reg(&y, "a", &[&ev('b')]);
    r_y.activity_token = r_x.activity_token.clone();
    let out = upsert(&pool, &y, &fcm_y, &r_y, 200, &ev('2'), FAR_DUE_MS).await;
    assert_eq!(out, UpsertOutcome::TokenOwnedElsewhere);

    // Y has nothing: no hash, job, device pointer or owner-set entry.
    let id_y = id_of(&r_y);
    assert!(hgetall(&pool, &format!("live_activity:{id_y}"))
        .await
        .is_empty());
    assert_eq!(job_score(&pool, &id_y).await, None);
    assert_eq!(get(&pool, &device_key(&y, &fcm_y)).await, None);
    assert!(smembers(&pool, &format!("live_activity_owner:{y}"))
        .await
        .is_empty());

    // X is unaffected, and the binding still names X.
    assert!(store::is_live(&pool, &id_x).await.unwrap());
    assert_eq!(job_score(&pool, &id_x).await, Some(FAR_DUE_MS));
    assert_eq!(
        get(&pool, &device_key(&x, &fcm_x)).await,
        Some(id_x.clone())
    );
    assert_eq!(
        get(&pool, &binding_key(&r_x.activity_token)).await,
        Some(x.clone())
    );

    // Once X ends it, the binding stays until its expiry: Y is still refused.
    let mut end_x = r_x.clone();
    end_x.state = ActivityState::Ended;
    store::end_by_token(&pool, &x, &end_x, 300, &ev('3'))
        .await
        .unwrap();
    assert_eq!(
        upsert(&pool, &y, &fcm_y, &r_y, 400, &ev('4'), FAR_DUE_MS).await,
        UpsertOutcome::TokenOwnedElsewhere
    );
}

#[tokio::test]
async fn binding_expires_with_the_activity() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let x = owner();
    let r = reg(&x, "a", &[&ev('a')]);
    upsert(&pool, &x, &hex(40), &r, 100, &ev('1'), FAR_DUE_MS).await;

    let horizon = (r.window_ends_at + 2100 - now_secs()) as i64;
    let binding = ttl(&pool, &binding_key(&r.activity_token)).await;
    let hash = ttl(&pool, &format!("live_activity:{}", id_of(&r))).await;
    assert!(
        (horizon - 5..=horizon).contains(&binding),
        "{binding} vs {horizon}"
    );
    assert!(
        (binding - hash).abs() <= 1,
        "binding {binding} vs hash {hash}"
    );

    store::finish(&pool, &id_of(&r)).await.unwrap();
}

#[tokio::test]
async fn same_owner_rotation_and_updates_still_work() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let x = owner();
    let fcm = hex(40);
    let first = reg(&x, "same", &[&ev('a')]);
    assert_eq!(
        upsert(&pool, &x, &fcm, &first, 100, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Created
    );
    assert_eq!(
        upsert(&pool, &x, &fcm, &first, 101, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Updated
    );

    let mut rotated = reg(&x, "same", &[&ev('a')]);
    rotated.posted_at = first.posted_at;
    rotated.window_ends_at = first.window_ends_at;
    assert_eq!(
        upsert(&pool, &x, &fcm, &rotated, 102, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Superseded {
            previous_id: id_of(&first)
        }
    );
    // The binding follows the new token; the old one keeps naming X until it expires.
    assert_eq!(
        get(&pool, &binding_key(&rotated.activity_token)).await,
        Some(x.clone())
    );
    assert_eq!(
        get(&pool, &binding_key(&first.activity_token)).await,
        Some(x.clone())
    );

    let y = owner();
    let mut steal = reg(&y, "same", &[&ev('b')]);
    steal.activity_token = rotated.activity_token.clone();
    assert_eq!(
        upsert(&pool, &y, &hex(40), &steal, 103, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::TokenOwnedElsewhere
    );

    store::finish(&pool, &id_of(&rotated)).await.unwrap();
}

#[tokio::test]
async fn update_cannot_extend_the_window() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let x = owner();
    let fcm = hex(40);
    let r = reg(&x, "a", &[&ev('a')]);
    let key = format!("live_activity:{}", id_of(&r));
    upsert(&pool, &x, &fcm, &r, 100, &ev('1'), FAR_DUE_MS).await;
    let horizon = (r.window_ends_at + 2100 - now_secs()) as i64;

    // Whoever holds the token sends a later window to renew the hash and binding.
    let mut later = r.clone();
    later.posted_at += 600;
    later.window_ends_at += 600;
    later.post_event_ids = vec![ev('b')];
    later.show_first_run_note = false;
    let new_fcm = hex(40);
    assert_eq!(
        upsert(&pool, &x, &new_fcm, &later, 101, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Updated
    );

    let h = hgetall(&pool, &key).await;
    assert_eq!(h["posted_at"], r.posted_at.to_string());
    assert_eq!(h["window_ends_at"], r.window_ends_at.to_string());
    for k in [
        key.clone(),
        binding_key(&r.activity_token),
        device_key(&x, &new_fcm),
    ] {
        let t = ttl(&pool, &k).await;
        assert!(
            (horizon - 5..=horizon).contains(&t),
            "{k}: {t} vs {horizon}"
        );
    }
    // Everything else an update carries still applies.
    assert_eq!(h["fcm_token"], new_fcm);
    assert_eq!(h["show_first_run_note"], "0");
    let ids: Vec<String> = serde_json::from_str(&h["post_event_ids"]).unwrap();
    assert_eq!(ids, vec![ev('a'), ev('b')]);

    store::finish(&pool, &id_of(&r)).await.unwrap();
}

#[tokio::test]
async fn rotation_keeps_the_original_window() {
    let Some(pool) = test_pool().await else {
        println!("Skipping test: Redis not available");
        return;
    };
    let x = owner();
    let fcm = hex(40);
    let first = reg(&x, "same", &[&ev('a')]);
    upsert(&pool, &x, &fcm, &first, 100, &ev('1'), FAR_DUE_MS).await;
    let horizon = (first.window_ends_at + 2100 - now_secs()) as i64;

    let mut rotated = reg(&x, "same", &[&ev('a')]);
    rotated.posted_at = first.posted_at + 600;
    rotated.window_ends_at = first.window_ends_at + 600;
    assert_eq!(
        upsert(&pool, &x, &fcm, &rotated, 101, &ev('1'), FAR_DUE_MS).await,
        UpsertOutcome::Superseded {
            previous_id: id_of(&first)
        }
    );

    let key = format!("live_activity:{}", id_of(&rotated));
    let h = hgetall(&pool, &key).await;
    assert_eq!(h["posted_at"], first.posted_at.to_string());
    assert_eq!(h["window_ends_at"], first.window_ends_at.to_string());
    for k in [
        key.clone(),
        binding_key(&rotated.activity_token),
        device_key(&x, &fcm),
    ] {
        let t = ttl(&pool, &k).await;
        assert!(
            (horizon - 5..=horizon).contains(&t),
            "{k}: {t} vs {horizon}"
        );
    }

    // A new post on the device is not a rotation: it takes its own window.
    let mut other = reg(&x, "other", &[&ev('c')]);
    other.posted_at = first.posted_at + 600;
    other.window_ends_at = first.window_ends_at + 600;
    upsert(&pool, &x, &fcm, &other, 102, &ev('1'), FAR_DUE_MS).await;
    let h = hgetall(&pool, &format!("live_activity:{}", id_of(&other))).await;
    assert_eq!(h["window_ends_at"], other.window_ends_at.to_string());

    store::finish(&pool, &id_of(&other)).await.unwrap();
}
