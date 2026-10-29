use divine_push_service::{
    activity::{self, ActivityState, ActivityUpdate},
    notification_delivery::{
        reserve_budget_for_generation, validate_reservation, BudgetDecision, DeliverySettings,
    },
    redis_store::{self, RedisPool},
};
use nostr_sdk::{Keys, PublicKey};
use uuid::Uuid;

async fn setup() -> (RedisPool, PublicKey) {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    (
        redis_store::create_pool(&url, 5).await.unwrap(),
        Keys::generate().public_key(),
    )
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
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        1
    );
    assert_eq!(
        spend(&pool, &owner, "while-active", 1, &settings).await,
        BudgetDecision::Active
    );
    update(&pool, &owner, session, 2, 1, ActivityState::Background).await;
    assert_eq!(
        spend(&pool, &owner, "new-activity", 1, &settings).await,
        BudgetDecision::Allowed
    );
    assert_eq!(
        spend(&pool, &owner, "first", 1, &settings).await,
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
    assert!(matches!(
        spend(&pool, &owner, "after-return", 1, &settings).await,
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
        spend(&pool, &owner, "after-return", 1, &settings).await,
        BudgetDecision::Allowed
    );
}

#[tokio::test]
async fn heartbeat_duplicate_and_reordered_activity_never_refill() {
    let (pool, owner) = setup().await;
    let session = Uuid::new_v4();
    update(&pool, &owner, session, 1, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Foreground).await;
    update(&pool, &owner, session, 2, 1, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        1
    );
    update(&pool, &owner, session, 4, 1, ActivityState::Background).await;
    update(&pool, &owner, session, 3, 1, ActivityState::Foreground).await;
    assert!(!activity::is_active(&pool, &owner).await.unwrap());
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        1
    );
    update(&pool, &owner, session, 5, 2, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        2
    );
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
        1
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
    assert!(validate_reservation(&pool, &owner, "leased", Some(0))
        .await
        .unwrap());
    update(
        &pool,
        &owner,
        Uuid::new_v4(),
        1,
        1,
        ActivityState::Foreground,
    )
    .await;
    assert!(!validate_reservation(&pool, &owner, "leased", Some(0))
        .await
        .unwrap());
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
    // Sequence2 was a background report lost while offline. The next real
    // foreground carries its own visit epoch, independent of delivery order.
    update(&pool, &owner, session, 3, 2, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        2
    );
    update(&pool, &owner, session, 4, 2, ActivityState::Foreground).await;
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        2
    );
    update(&pool, &owner, session, 5, 2, ActivityState::Background).await;
    update(&pool, &owner, session, 6, 1, ActivityState::Foreground).await;
    assert!(!activity::is_active(&pool, &owner).await.unwrap());
    assert_eq!(
        activity::current_generation(&pool, &owner).await.unwrap(),
        2
    );
}
