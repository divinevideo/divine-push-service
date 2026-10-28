use divine_push_service::{
    notification_delivery::{in_quiet_hours, reserve_budget, DeliverySettings},
    redis_store,
};
use nostr_sdk::Keys;

#[test]
fn quiet_hours_follow_device_offset_and_midnight_boundaries() {
    let settings = DeliverySettings::default();
    // 22:00 UTC, 21:59 UTC, 08:00 UTC, 07:59 UTC.
    assert!(in_quiet_hours(0, 79_200, &settings));
    assert!(!in_quiet_hours(0, 79_199, &settings));
    assert!(!in_quiet_hours(0, 28_800, &settings));
    assert!(in_quiet_hours(0, 28_799, &settings));
    assert!(in_quiet_hours(330, 59_400, &settings));
    assert!(!in_quiet_hours(-480, 59_400, &settings));
}

#[tokio::test]
async fn recipient_budget_is_atomic_shared_and_retry_idempotent() {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let pool = redis_store::create_pool(&url, 5)
        .await
        .expect("test Redis is required");
    let recipient = Keys::generate().public_key();
    let settings = DeliverySettings {
        hourly_cap: 2,
        daily_cap: 3,
        ..Default::default()
    };
    let (a, b, c) = tokio::join!(
        reserve_budget(&pool, &recipient, "social:a", &settings),
        reserve_budget(&pool, &recipient, "summary:b", &settings),
        reserve_budget(&pool, &recipient, "campaign:c", &settings),
    );
    assert_eq!(
        [a.unwrap(), b.unwrap(), c.unwrap()]
            .iter()
            .filter(|allowed| **allowed)
            .count(),
        2
    );
    let mut conn = pool.get().await.unwrap();
    let key = format!("delivery_budget:{}", recipient.to_hex());
    let ids: Vec<String> = redis::cmd("ZRANGE")
        .arg(&key)
        .arg(0)
        .arg(-1)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(reserve_budget(&pool, &recipient, &ids[0], &settings)
        .await
        .unwrap());
    // Age both reservations outside the hour, but inside the day.
    let now: (u64, u64) = redis::cmd("TIME").query_async(&mut *conn).await.unwrap();
    for id in ids {
        redis::cmd("ZADD")
            .arg(&key)
            .arg(now.0 - 3601)
            .arg(id)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }
    assert!(reserve_budget(&pool, &recipient, "new-hour", &settings)
        .await
        .unwrap());
    assert!(
        !reserve_budget(&pool, &recipient, "daily-overflow", &settings)
            .await
            .unwrap()
    );
    redis::cmd("DEL")
        .arg(key)
        .query_async::<()>(&mut *conn)
        .await
        .unwrap();
}

#[test]
fn named_timezone_follows_dst_without_a_new_registration() {
    use divine_push_service::redis_store::timezone_offset_at;
    let instant = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .timestamp()
    };
    assert_eq!(
        timezone_offset_at("America/New_York", instant("2026-03-08T06:59:59Z")),
        Some(-300)
    );
    assert_eq!(
        timezone_offset_at("America/New_York", instant("2026-03-08T07:00:00Z")),
        Some(-240)
    );
    assert_eq!(
        timezone_offset_at("America/New_York", instant("2026-11-01T05:59:59Z")),
        Some(-240)
    );
    assert_eq!(
        timezone_offset_at("America/New_York", instant("2026-11-01T06:00:00Z")),
        Some(-300)
    );
    assert_eq!(timezone_offset_at("not-a-zone", 0), None);
}

#[tokio::test]
async fn quiet_device_does_not_silence_awake_or_legacy_devices() {
    use divine_push_service::notification_delivery::social_tokens;
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let pool = redis_store::create_pool(&url, 5).await.unwrap();
    let recipient = Keys::generate().public_key();
    let hour = (chrono::Utc::now().timestamp().rem_euclid(86400) / 3600) as i32;
    let offset = |target: i32| ((target - hour + 12).rem_euclid(24) - 12) * 60;
    redis_store::add_or_update_token_with_timezone(
        &pool,
        &recipient,
        "synthetic-quiet-device",
        Some(offset(23)),
    )
    .await
    .unwrap();
    redis_store::add_or_update_token_with_timezone(
        &pool,
        &recipient,
        "synthetic-awake-device",
        Some(offset(12)),
    )
    .await
    .unwrap();
    redis_store::add_or_update_token(&pool, &recipient, "synthetic-legacy-device")
        .await
        .unwrap();
    let mut tokens = social_tokens(&pool, &recipient, &DeliverySettings::default())
        .await
        .unwrap();
    tokens.sort();
    assert_eq!(
        tokens,
        ["synthetic-awake-device", "synthetic-legacy-device"]
    );
    for token in [
        "synthetic-quiet-device",
        "synthetic-awake-device",
        "synthetic-legacy-device",
    ] {
        redis_store::remove_token(&pool, &recipient, token)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn named_registration_overrides_offset_and_is_removed_with_token() {
    use divine_push_service::redis_store::{
        add_or_update_token_with_timezone_name, get_tokens_with_timezone_offsets,
        timezone_offset_at,
    };
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let pool = redis_store::create_pool(&url, 5).await.unwrap();
    let recipient = Keys::generate().public_key();
    let token = format!("synthetic-zone-{}", recipient.to_hex());
    add_or_update_token_with_timezone_name(
        &pool,
        &recipient,
        &token,
        Some(0),
        Some("Pacific/Auckland"),
    )
    .await
    .unwrap();
    let devices = get_tokens_with_timezone_offsets(&pool, &recipient)
        .await
        .unwrap();
    assert_eq!(
        devices[0].1,
        timezone_offset_at("Pacific/Auckland", chrono::Utc::now().timestamp())
    );
    add_or_update_token_with_timezone_name(
        &pool,
        &recipient,
        &token,
        Some(120),
        Some("invalid-zone"),
    )
    .await
    .unwrap();
    assert_eq!(
        get_tokens_with_timezone_offsets(&pool, &recipient)
            .await
            .unwrap()[0]
            .1,
        Some(120)
    );
    redis_store::remove_token(&pool, &recipient, &token)
        .await
        .unwrap();
    let mut conn = pool.get().await.unwrap();
    let name: Option<String> = redis::cmd("HGET")
        .arg("token_timezone_names")
        .arg(token)
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(name.is_none());
}
