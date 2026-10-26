//! Shared delivery limits for social pushes, summaries, and campaigns.
//!
//! Suppression affects pushes only; activity remains in the in-app inbox. Missing
//! offsets retain social delivery for old clients. Campaign consent/timezone
//! requirements are enforced separately and are never relaxed by this module.

use crate::{
    error::{Result, ServiceError},
    models::FcmPayload,
    redis_store::{self, RedisPool},
};
use nostr_sdk::PublicKey;
use serde::Deserialize;

/// Recipient-local quiet hours and recipient-wide rolling send budgets.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeliverySettings {
    pub quiet_start_hour: u32,
    pub quiet_end_hour: u32,
    pub hourly_cap: u64,
    pub daily_cap: u64,
    pub max_event_age_secs: u64,
}

impl Default for DeliverySettings {
    fn default() -> Self {
        Self {
            quiet_start_hour: 22,
            quiet_end_hour: 8,
            hourly_cap: 5,
            daily_cap: 20,
            max_event_age_secs: 3600,
        }
    }
}

/// Whether the current device-local hour falls within quiet hours.
pub fn in_quiet_hours(offset_minutes: i32, now: i64, settings: &DeliverySettings) -> bool {
    let hour = ((now + i64::from(offset_minutes) * 60).rem_euclid(86_400) / 3600) as u32;
    if settings.quiet_start_hour > settings.quiet_end_hour {
        hour >= settings.quiet_start_hour || hour < settings.quiet_end_hour
    } else {
        hour >= settings.quiet_start_hour && hour < settings.quiet_end_hour
    }
}

/// Social pushes remain eligible on legacy registrations without a timezone.
/// Quiet work is suppressed rather than replayed as a morning burst.
pub async fn social_tokens(
    pool: &RedisPool,
    recipient: &PublicKey,
    settings: &DeliverySettings,
) -> Result<Vec<String>> {
    let now = chrono::Utc::now().timestamp();
    let devices = redis_store::get_tokens_with_timezone_offsets(pool, recipient).await?;
    let mut tokens = Vec::new();
    for (token, offset) in devices {
        if offset.is_some_and(|offset| in_quiet_hours(offset, now, settings)) {
            outcome("quiet_hours");
        } else {
            if offset.is_none() {
                outcome("legacy_timezone_unknown");
            }
            tokens.push(token);
        }
    }
    Ok(tokens)
}

/// Atomically reserve one recipient-wide delivery slot across replicas and all
/// categories. Retries with the same identity consume no additional slot. Failed
/// or ambiguous provider attempts conservatively retain their reservation.
pub async fn reserve_budget(
    pool: &RedisPool,
    recipient: &PublicKey,
    identity: &str,
    settings: &DeliverySettings,
) -> Result<bool> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?;
    let allowed: bool = redis::Script::new(r#"
        local now = tonumber(redis.call('TIME')[1])
        redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now - 86400)
        if redis.call('ZSCORE', KEYS[1], ARGV[1]) then return 1 end
        if redis.call('ZCARD', KEYS[1]) >= tonumber(ARGV[3]) then return 0 end
        if redis.call('ZCOUNT', KEYS[1], '(' .. (now - 3600), '+inf') >= tonumber(ARGV[2]) then return 0 end
        redis.call('ZADD', KEYS[1], now, ARGV[1])
        redis.call('EXPIRE', KEYS[1], 86400)
        return 1
    "#).key(format!("delivery_budget:{}", recipient.to_hex()))
        .arg(identity).arg(settings.hourly_cap).arg(settings.daily_cap)
        .invoke_async(&mut *conn).await.map_err(ServiceError::Redis)?;
    if !allowed {
        outcome("frequency_capped");
    }
    Ok(allowed)
}

/// Keep provider storage short so offline devices do not replay old activity.
/// Zero TTL asks both providers to deliver immediately or discard the push;
/// social history is always available in the inbox. This also prevents a push
/// accepted just before quiet hours from waiting at the provider overnight.
pub fn immediate_delivery(payload: &mut FcmPayload) {
    if payload
        .data
        .as_ref()
        .and_then(|data| data.get("type"))
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "like" | "comment" | "mention" | "repost" | "newPost"
            )
        })
    {
        payload
            .data
            .as_mut()
            .expect("data exists")
            .insert("prefetchContent".into(), "true".into());
    }
    payload
        .data
        .get_or_insert_with(Default::default)
        .insert("deliveryTtlSeconds".into(), "0".into());
}

pub(crate) fn outcome(reason: &'static str) {
    metrics::counter!("push_delivery_policy_total", "outcome" => reason).increment(1);
}
