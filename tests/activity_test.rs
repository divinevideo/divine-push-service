use divine_push_service::{
    activity::{self, ActivityState, ActivityUpdate},
    notification_delivery::{
        reserve_budget_for_generation, validate_reservation, BudgetDecision, DeliverySettings,
    },
    redis_store::{self, RedisPool},
};
use nostr_sdk::{Keys, PublicKey};
use uuid::Uuid;

async fn setup_unregistered() -> (RedisPool, PublicKey) {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    (
        redis_store::create_pool(&url, 5).await.unwrap(),
        Keys::generate().public_key(),
    )
}

async fn setup() -> (RedisPool, PublicKey) {
    let (pool, owner) = setup_unregistered().await;
    redis_store::add_or_update_token(
        &pool,
        &owner,
        &format!("synthetic-activity-{}", owner.to_hex()),
    )
    .await
    .unwrap();
    (pool, owner)
}

async fn update(
    pool: &RedisPool,
    owner: &PublicKey,
    session: Uuid,
    sequence: u32,
    visit: u32,
    state: ActivityState,
) {
    activity::record_activity(
        pool,
        owner,
        &ActivityUpdate {
            session_id: session,
            sequence,
            visit,
            state,
        },
    )
    .await
    .unwrap();
}

async fn spend(
    pool: &RedisPool,
    owner: &PublicKey,
    id: &str,
    generation: u64,
    settings: &DeliverySettings,
) -> BudgetDecision {
    reserve_budget_for_generation(pool, owner, id, settings, Some(generation))
        .await
        .unwrap()
}

#[tokio::test]
async fn returning_refills_allowance_without_erasing_delivery_history() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings {
        min_interval_secs: 0,
        ..Default::default()
    };
    assert_eq!(
        spend(&pool, &owner, "first", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "second", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "third", 0, &settings).await,
        BudgetDecision::AllowanceExhausted
    );
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let generation = activity::current_generation(&pool, &owner).await.unwrap();
    assert_ne!(generation, 0);
    assert_eq!(
        spend(&pool, &owner, "while-active", generation, &settings).await,
        BudgetDecision::Active
    );
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    assert_eq!(
        spend(&pool, &owner, "new-activity", generation, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "first", generation, &settings).await,
        BudgetDecision::StaleVisit
    );
    assert_eq!(
        spend(&pool, &owner, "queued-before-open", 0, &settings).await,
        BudgetDecision::StaleVisit
    );
}

#[tokio::test]
async fn a_return_does_not_bypass_six_hour_cooldown() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    assert_eq!(
        spend(&pool, &owner, "noon", 0, &settings).await,
        BudgetDecision::Allowed
    );
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    let generation = activity::current_generation(&pool, &owner).await.unwrap();
    assert!(matches!(
        spend(&pool, &owner, "after-return", generation, &settings).await,
        BudgetDecision::Cooldown {
            retry_after_secs: 21599..=21600
        }
    ));
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_budget:{}", owner.to_hex()))
        .arg(now.0 - 21600)
        .arg("noon")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        spend(&pool, &owner, "after-return", generation, &settings).await,
        BudgetDecision::Allowed
    );
}

#[tokio::test]
async fn heartbeat_duplicate_and_reordered_activity_never_refill() {
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let first_visit = activity::current_generation(&pool, &owner).await.unwrap();
    update(&pool, &owner, session, 2, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        first_visit
    );
    update(&pool, &owner, session, 4, 1, ActivityState::Background).await;
    update(&pool, &owner, session, 3, 1, ActivityState::Foreground).await;
    assert!(!activity::is_active(&pool, &owner).await.unwrap());
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        first_visit
    );
    update(&pool, &owner, session, 5, 2, ActivityState::Foreground).await;
    assert!(activity::current_generation(&pool, &owner).await.unwrap() > first_visit);
}

#[tokio::test]
async fn one_background_device_does_not_clear_another_active_device() {
    let (pool, owner) = setup().await;
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    update(&pool, &owner, a, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, b, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, a, 2, 1, ActivityState::Background).await;
    assert!(activity::is_active(&pool, &owner).await.unwrap());
    update(&pool, &owner, b, 2, 1, ActivityState::Background).await;
    assert!(!activity::is_active(&pool, &owner).await.unwrap());
}

#[tokio::test]
async fn crashed_presence_expires_and_heartbeat_does_not_create_a_visit() {
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let generation = activity::current_generation(&pool, &owner).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("{}{}", activity::ACTIVE_PREFIX, owner.to_hex()))
        .arg(0)
        .arg(session.to_string())
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    assert!(!activity::is_active(&pool, &owner).await.unwrap());
    update(&pool, &owner, session, 2, 1, ActivityState::Foreground).await;
    assert!(activity::is_active(&pool, &owner).await.unwrap());
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        generation
    );
}

#[tokio::test]
async fn concurrent_categories_share_one_cooldown_slot() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    let (a, b, c) = tokio::join!(
        spend(&pool, &owner, "social", 0, &settings),
        spend(&pool, &owner, "summary", 0, &settings),
        spend(&pool, &owner, "campaign", 0, &settings),
    );
    assert_eq!(
        [a, b, c]
            .iter()
            .filter(|decision| **decision == BudgetDecision::Allowed)
            .count(),
        1
    );
}

#[tokio::test]
async fn a_visit_invalidates_a_reserved_job_before_provider_submission() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    assert_eq!(
        spend(&pool, &owner, "leased", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert!(
        validate_reservation(&pool, &owner, "leased", &settings, Some(0))
            .await
            .unwrap()
    );
    update(
        &pool,
        &owner,
        Uuid::new_v4(),
        1,
        1,
        ActivityState::Foreground,
    )
    .await;
    assert!(
        !validate_reservation(&pool, &owner, "leased", &settings, Some(0))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn same_identity_does_not_spend_a_second_allowance() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings {
        min_interval_secs: 0,
        ..Default::default()
    };
    assert_eq!(
        spend(&pool, &owner, "retry", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "retry", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "second", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "third", 0, &settings).await,
        BudgetDecision::AllowanceExhausted
    );
}

#[tokio::test]
async fn missed_background_does_not_prevent_the_next_visit_reset() {
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let first_visit = activity::current_generation(&pool, &owner).await.unwrap();
    // Sequence2 was a background report lost while offline. The next real
    // foreground carries its own visit epoch, independent of delivery order.
    update(&pool, &owner, session, 3, 2, ActivityState::Foreground).await;
    let second_visit = activity::current_generation(&pool, &owner).await.unwrap();
    assert!(second_visit > first_visit);
    update(&pool, &owner, session, 4, 2, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        second_visit
    );
    update(&pool, &owner, session, 5, 2, ActivityState::Background).await;
    update(&pool, &owner, session, 6, 1, ActivityState::Foreground).await;
    assert!(!activity::is_active(&pool, &owner).await.unwrap());
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        second_visit
    );
}

#[tokio::test]
async fn delayed_retry_cannot_bypass_a_newer_notifications_cooldown() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    assert_eq!(
        spend(&pool, &owner, "old-retry", 0, &settings).await,
        BudgetDecision::Allowed
    );
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_budget:{}", owner.to_hex()))
        .arg(now.0 - 21601)
        .arg("old-retry")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(&pool, &owner, "newer-send", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert!(matches!(
        spend(&pool, &owner, "old-retry", 0, &settings).await,
        BudgetDecision::Cooldown {
            retry_after_secs: 21599..=21600
        }
    ));
}

#[tokio::test]
async fn a_delayed_retry_refreshes_the_gap_for_the_next_notification() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    assert_eq!(
        spend(&pool, &owner, "retry", 0, &settings).await,
        BudgetDecision::Allowed
    );
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_budget:{}", owner.to_hex()))
        .arg(now.0 - 21601)
        .arg("retry")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(&pool, &owner, "retry", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert!(matches!(
        spend(&pool, &owner, "next", 0, &settings).await,
        BudgetDecision::Cooldown {
            retry_after_secs: 21599..=21600
        }
    ));
}

#[tokio::test]
async fn preflight_retries_keep_the_first_work_generation_without_spending_allowance() {
    let (pool, owner) = setup().await;
    redis_store::add_or_update_token(
        &pool,
        &owner,
        &format!("synthetic-bound-work-{}", owner.to_hex()),
    )
    .await
    .unwrap();
    let first = activity::generation_for_work(&pool, &owner, "campaign:pending", 86400)
        .await
        .unwrap();
    assert_ne!(first, 0);
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    let retry = activity::generation_for_work(&pool, &owner, "campaign:pending", 86400)
        .await
        .unwrap();
    assert_eq!(retry, first);
    let settings = DeliverySettings::default();
    assert_eq!(
        spend(&pool, &owner, "campaign:pending", retry, &settings).await,
        BudgetDecision::StaleVisit
    );
    let new_work = activity::generation_for_work(&pool, &owner, "campaign:new", 86400)
        .await
        .unwrap();
    assert_eq!(
        new_work,
        activity::current_generation(&pool, &owner).await.unwrap()
    );
    assert_ne!(new_work, first);
    assert_eq!(
        spend(&pool, &owner, "campaign:new", new_work, &settings).await,
        BudgetDecision::Allowed
    );
}

#[tokio::test]
async fn provider_preflight_rejects_an_old_reservation_after_a_newer_send() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    assert_eq!(
        spend(&pool, &owner, "old", 0, &settings).await,
        BudgetDecision::Allowed
    );
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_budget:{}", owner.to_hex()))
        .arg(now.0 - 21601)
        .arg("old")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(&pool, &owner, "new", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert!(
        !validate_reservation(&pool, &owner, "old", &settings, Some(0))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn activity_without_registered_devices_creates_no_presence_or_visit_state() {
    let (pool, owner) = setup_unregistered().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let mut conn = pool.get().await.unwrap();
    let keys: u64 = redis::cmd("EXISTS")
        .arg(format!("notification_activity:{}", owner.to_hex()))
        .arg(format!("notification_active:{}", owner.to_hex()))
        .arg(format!(
            "notification_activity_session:{}:{session}",
            owner.to_hex()
        ))
        .arg(format!("notification_activity_sessions:{}", owner.to_hex()))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        keys, 0,
        "an account without a device has no push activity to retain"
    );
    drop(conn);
    redis_store::add_or_update_token(
        &pool,
        &owner,
        &format!("synthetic-retention-{}", owner.to_hex()),
    )
    .await
    .unwrap();
    // An ignored report must not consume the sequence before registration.
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    assert!(activity::is_active(&pool, &owner).await.unwrap());
    assert_ne!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn accepted_activity_renews_ninety_day_retention_but_stale_reports_do_not() {
    let (pool, owner) = setup().await;
    redis_store::add_or_update_token(
        &pool,
        &owner,
        &format!("synthetic-retention-{}", owner.to_hex()),
    )
    .await
    .unwrap();
    let session = Uuid::new_v4();
    let key = format!("notification_activity:{}", owner.to_hex());
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let mut conn = pool.get().await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(
        (7_775_999..=7_776_000).contains(&ttl),
        "activity state must expire after 90 inactive days, got {ttl}"
    );
    redis::cmd("EXPIRE")
        .arg(&key)
        .arg(60)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let mut conn = pool.get().await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!((1..=60).contains(&ttl), "a replay must not renew retention");
    drop(conn);
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    let mut conn = pool.get().await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(
        (7_775_999..=7_776_000).contains(&ttl),
        "accepted reports must renew retention"
    );
    redis::cmd("EXPIRE")
        .arg(&key)
        .arg(60)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let generation = activity::current_generation(&pool, &owner).await.unwrap();
    assert_eq!(
        spend(
            &pool,
            &owner,
            "after-activity",
            generation,
            &DeliverySettings::default()
        )
        .await,
        BudgetDecision::Allowed
    );
    let mut conn = pool.get().await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(
        (1..=60).contains(&ttl),
        "sending a push must not extend activity retention"
    );
}

#[tokio::test]
async fn delivery_without_activity_never_creates_immortal_activity_state() {
    let (pool, owner) = setup().await;
    assert_eq!(
        spend(
            &pool,
            &owner,
            "delivery-only",
            0,
            &DeliverySettings::default()
        )
        .await,
        BudgetDecision::Allowed
    );
    let mut conn = pool.get().await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(format!("notification_activity:{}", owner.to_hex()))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert_ne!(
        ttl, -1,
        "delivery must not leave activity state without an expiry"
    );
}

#[tokio::test]
async fn activity_keeps_a_reporting_flag_instead_of_an_unused_timestamp() {
    let (pool, owner) = setup().await;
    let key = format!("notification_activity:{}", owner.to_hex());
    let mut conn = pool.get().await.unwrap();
    redis::cmd("HSET")
        .arg(&key)
        .arg("last_activity_at")
        .arg(1)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    update(
        &pool,
        &owner,
        Uuid::new_v4(),
        1,
        1,
        ActivityState::Foreground,
    )
    .await;
    let mut conn = pool.get().await.unwrap();
    let flag: Option<u8> = redis::cmd("HGET")
        .arg(&key)
        .arg("activity_reported")
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert_eq!(flag, Some(1));
    let timestamp_exists: bool = redis::cmd("HEXISTS")
        .arg(&key)
        .arg("last_activity_at")
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(
        !timestamp_exists,
        "activity should no longer retain the unused timestamp"
    );
}

#[tokio::test]
async fn expired_activity_cannot_revalidate_work_from_an_earlier_visit() {
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    // Campaign work can live longer than the activity hash: its retention
    // includes the remaining campaign lifetime, not just the seven-day floor.
    let old = activity::generation_for_work(
        &pool,
        &owner,
        "campaign:long-lived-preflight",
        activity::STATE_TTL_SECS * 2,
    )
    .await
    .unwrap();
    update(&pool, &owner, session, 3, 2, ActivityState::Foreground).await;
    update(&pool, &owner, session, 4, 2, ActivityState::Background).await;
    let mut conn = pool.get().await.unwrap();
    // Model the account hash expiring while the longer-lived work marker
    // remains. A returning process has a new session after the long absence.
    redis::cmd("EXPIRE")
        .arg(format!("notification_activity:{}", owner.to_hex()))
        .arg(0)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let returning_session = Uuid::new_v4();
    update(
        &pool,
        &owner,
        returning_session,
        1,
        1,
        ActivityState::Foreground,
    )
    .await;
    update(
        &pool,
        &owner,
        returning_session,
        2,
        1,
        ActivityState::Background,
    )
    .await;
    let retry = activity::generation_for_work(
        &pool,
        &owner,
        "campaign:long-lived-preflight",
        activity::STATE_TTL_SECS,
    )
    .await
    .unwrap();
    assert_eq!(retry, old, "the first-attempt binding must survive");
    assert_eq!(
        spend(
            &pool,
            &owner,
            "campaign:long-lived-preflight",
            retry,
            &DeliverySettings::default(),
        )
        .await,
        BudgetDecision::StaleVisit,
        "expiring activity state must not recycle an old visit generation"
    );
}

#[tokio::test]
async fn pre_visit_work_cannot_match_missing_activity_after_retention_expires() {
    let (pool, owner) = setup().await;
    let identity = "campaign:pre-visit-long-lived";
    let first =
        activity::generation_for_work(&pool, &owner, identity, activity::STATE_TTL_SECS * 2)
            .await
            .unwrap();
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    let mut conn = pool.get().await.unwrap();
    redis::cmd("EXPIRE")
        .arg(format!("notification_activity:{}", owner.to_hex()))
        .arg(0)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let retry = activity::generation_for_work(&pool, &owner, identity, activity::STATE_TTL_SECS)
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(
        spend(&pool, &owner, identity, retry, &DeliverySettings::default()).await,
        BudgetDecision::StaleVisit
    );
    let new_identity = "campaign:new-after-retention";
    let current = activity::generation_for_work(&pool, &owner, new_identity, 86400)
        .await
        .unwrap();
    assert_ne!(current, first);
    assert_eq!(
        spend(
            &pool,
            &owner,
            new_identity,
            current,
            &DeliverySettings::default()
        )
        .await,
        BudgetDecision::Allowed
    );
}

#[tokio::test]
async fn account_session_limit_rejects_new_ids_without_resetting_existing_state() {
    let (pool, owner) = setup().await;
    let mut sessions = Vec::new();
    // A week's retained sessions are bounded independently of the IP limiter.
    for _ in 0..activity::MAX_SESSIONS_PER_ACCOUNT {
        let session = Uuid::new_v4();
        update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
        sessions.push(session);
    }
    let before = activity::current_generation(&pool, &owner).await.unwrap();
    let extra = Uuid::new_v4();
    assert!(activity::record_activity(
        &pool,
        &owner,
        &ActivityUpdate {
            session_id: extra,
            sequence: 1,
            visit: 1,
            state: ActivityState::Foreground,
        },
    )
    .await
    .is_err());
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        before
    );
    let mut conn = pool.get().await.unwrap();
    let exists: bool = redis::cmd("EXISTS")
        .arg(format!(
            "notification_activity_session:{}:{extra}",
            owner.to_hex()
        ))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(!exists, "a refused session must create no session hash");
    let active: u64 = redis::cmd("ZCARD")
        .arg(format!("notification_active:{}", owner.to_hex()))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert_eq!(active, activity::MAX_SESSIONS_PER_ACCOUNT);
    drop(conn);
    // An admitted session can still heartbeat at the cap without refilling.
    update(&pool, &owner, sessions[0], 2, 1, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn event_work_retention_uses_the_callers_retry_horizon() {
    let (pool, owner) = setup().await;
    let identity = "event:synthetic-retention";
    activity::generation_for_work(&pool, &owner, identity, 90_000)
        .await
        .unwrap();
    let mut conn = pool.get().await.unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(format!(
            "notification_work_generation:{}:{}",
            owner.to_hex(),
            blake3::hash(identity.as_bytes()).to_hex()
        ))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(
        (89_999..=90_000).contains(&ttl),
        "unexpected work TTL: {ttl}"
    );
}

#[tokio::test]
async fn expired_session_slots_are_reclaimed_atomically_under_concurrent_admission() {
    let (pool, owner) = setup().await;
    let mut first = None;
    for _ in 0..activity::MAX_SESSIONS_PER_ACCOUNT {
        let session = Uuid::new_v4();
        first.get_or_insert(session);
        update(&pool, &owner, session, 1, 1, ActivityState::Background).await;
    }
    let expired = first.unwrap();
    let index = format!("notification_activity_sessions:{}", owner.to_hex());
    let mut conn = pool.get().await.unwrap();
    redis::pipe()
        .atomic()
        .cmd("ZADD")
        .arg(&index)
        .arg(0)
        .arg(expired.to_string())
        .ignore()
        .cmd("EXPIRE")
        .arg(format!(
            "notification_activity_session:{}:{expired}",
            owner.to_hex()
        ))
        .arg(0)
        .ignore()
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let updates: Vec<_> = (0..2)
        .map(|_| ActivityUpdate {
            session_id: Uuid::new_v4(),
            sequence: 1,
            visit: 1,
            state: ActivityState::Foreground,
        })
        .collect();
    let (a, b) = tokio::join!(
        activity::record_activity(&pool, &owner, &updates[0]),
        activity::record_activity(&pool, &owner, &updates[1]),
    );
    assert_eq!(
        [a.is_ok(), b.is_ok()].into_iter().filter(|ok| *ok).count(),
        1
    );
    let mut conn = pool.get().await.unwrap();
    let count: u64 = redis::cmd("ZCARD")
        .arg(index)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, activity::MAX_SESSIONS_PER_ACCOUNT);
}

#[tokio::test]
async fn nonreporting_accounts_use_a_sliding_daily_allowance() {
    let (pool, owner) = setup().await;
    let settings = DeliverySettings {
        min_interval_secs: 0,
        ..Default::default()
    };
    let generation = activity::generation_for_work(&pool, &owner, "legacy", 86400)
        .await
        .unwrap();
    assert_ne!(generation, 0, "work epochs do not imply activity reporting");
    assert_eq!(
        spend(&pool, &owner, "first", generation, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "second", generation, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "third", generation, &settings).await,
        BudgetDecision::AllowanceExhausted
    );
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_budget:{}", owner.to_hex()))
        .arg(now.0 - 86401)
        .arg("first")
        .arg(now.0 - 21601)
        .arg("second")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(&pool, &owner, "third", generation, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "fourth", generation, &settings).await,
        BudgetDecision::AllowanceExhausted
    );
}

#[tokio::test]
async fn nonreporting_accounts_ignore_a_preexisting_sticky_absence_counter() {
    let (pool, owner) = setup().await;
    let mut conn = pool.get().await.unwrap();
    redis::cmd("HSET")
        .arg(format!("notification_activity:{}", owner.to_hex()))
        .arg("absence_used")
        .arg(2)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(
            &pool,
            &owner,
            "legacy-upgrade",
            0,
            &DeliverySettings::default()
        )
        .await,
        BudgetDecision::Allowed
    );
}

#[tokio::test]
async fn reporting_accounts_keep_the_absence_allowance_after_the_rolling_day() {
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    let generation = activity::current_generation(&pool, &owner).await.unwrap();
    let settings = DeliverySettings {
        min_interval_secs: 0,
        ..Default::default()
    };
    for id in ["first", "second"] {
        assert_eq!(
            spend(&pool, &owner, id, generation, &settings).await,
            BudgetDecision::Allowed
        );
    }
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_budget:{}", owner.to_hex()))
        .arg(now.0 - 86401)
        .arg("first")
        .arg(now.0 - 86401)
        .arg("second")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(&pool, &owner, "third", generation, &settings).await,
        BudgetDecision::AllowanceExhausted
    );
}

#[tokio::test]
async fn direct_message_reservations_keep_presence_and_visit_fences() {
    use divine_push_service::notification_delivery::{
        reserve_budget_for_class, validate_reservation_for_class, DeliveryClass,
    };
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    let settings = DeliverySettings::default();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    let generation = activity::current_generation(&pool, &owner).await.unwrap();
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm",
            &settings,
            Some(generation),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Active
    );
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm",
            &settings,
            Some(generation),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Allowed
    );
    assert!(validate_reservation_for_class(
        &pool,
        &owner,
        "dm",
        &settings,
        Some(generation),
        DeliveryClass::DirectMessage
    )
    .await
    .unwrap());
    update(&pool, &owner, session, 3, 2, ActivityState::Foreground).await;
    update(&pool, &owner, session, 4, 2, ActivityState::Background).await;
    assert!(!validate_reservation_for_class(
        &pool,
        &owner,
        "dm",
        &settings,
        Some(generation),
        DeliveryClass::DirectMessage
    )
    .await
    .unwrap());
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm",
            &settings,
            Some(generation),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::StaleVisit
    );
}

#[tokio::test]
async fn direct_messages_share_hourly_and_daily_bounds_with_nonurgent_pushes() {
    use divine_push_service::notification_delivery::{reserve_budget_for_class, DeliveryClass};
    let (pool, owner) = setup().await;
    let settings = DeliverySettings {
        hourly_cap: 2,
        daily_cap: 3,
        ..Default::default()
    };
    assert_eq!(
        spend(&pool, &owner, "social", 0, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm:first",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Allowed
    );
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm:second",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Capped
    );
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    for (key, id) in [
        ("delivery_budget", "social"),
        ("delivery_dm_budget", "dm:first"),
    ] {
        redis::cmd("ZADD")
            .arg(format!("{key}:{}", owner.to_hex()))
            .arg(now.0 - 3601)
            .arg(id)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }
    drop(conn);
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm:second",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Allowed
    );
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "dm:third",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Capped
    );
}

async fn dm_retry_outside_a_full_shared_hour() -> (RedisPool, PublicKey, DeliverySettings) {
    use divine_push_service::notification_delivery::{reserve_budget_for_class, DeliveryClass};
    let (pool, owner) = setup().await;
    let settings = DeliverySettings::default();
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "old-dm",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Allowed
    );
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_dm_budget:{}", owner.to_hex()))
        .arg(now.0 - 3601)
        .arg("old-dm")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        spend(&pool, &owner, "current-social", 0, &settings).await,
        BudgetDecision::Allowed
    );
    for i in 0..4 {
        assert_eq!(
            reserve_budget_for_class(
                &pool,
                &owner,
                &format!("current-dm:{i}"),
                &settings,
                Some(0),
                DeliveryClass::DirectMessage
            )
            .await
            .unwrap(),
            BudgetDecision::Allowed
        );
    }
    (pool, owner, settings)
}

async fn free_one_dm_hour_slot(pool: &RedisPool, owner: &PublicKey) {
    let mut conn = pool.get().await.unwrap();
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    redis::cmd("ZADD")
        .arg(format!("delivery_dm_budget:{}", owner.to_hex()))
        .arg(now.0 - 3601)
        .arg("current-dm:0")
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn dm_reservation_retry_cannot_reenter_a_full_hour() {
    use divine_push_service::notification_delivery::{reserve_budget_for_class, DeliveryClass};
    let (pool, owner, settings) = dm_retry_outside_a_full_shared_hour().await;
    assert_eq!(
        reserve_budget_for_class(
            &pool,
            &owner,
            "old-dm",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap(),
        BudgetDecision::Capped
    );
    free_one_dm_hour_slot(&pool, &owner).await;
    for _ in 0..2 {
        assert_eq!(
            reserve_budget_for_class(
                &pool,
                &owner,
                "old-dm",
                &settings,
                Some(0),
                DeliveryClass::DirectMessage
            )
            .await
            .unwrap(),
            BudgetDecision::Allowed,
            "a retry already inside the full hour consumes no new slot"
        );
    }
}

#[tokio::test]
async fn dm_provider_preflight_cannot_reenter_a_full_hour() {
    use divine_push_service::notification_delivery::{
        validate_reservation_for_class, DeliveryClass,
    };
    let (pool, owner, settings) = dm_retry_outside_a_full_shared_hour().await;
    assert!(!validate_reservation_for_class(
        &pool,
        &owner,
        "old-dm",
        &settings,
        Some(0),
        DeliveryClass::DirectMessage
    )
    .await
    .unwrap());
    free_one_dm_hour_slot(&pool, &owner).await;
    for _ in 0..2 {
        assert!(validate_reservation_for_class(
            &pool,
            &owner,
            "old-dm",
            &settings,
            Some(0),
            DeliveryClass::DirectMessage
        )
        .await
        .unwrap());
    }
}
