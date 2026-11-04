//! Shared delivery limits for social pushes, summaries, and campaigns.
//!
//! Suppression affects pushes only; it does not create durable inbox entries.
//! Unknown timezone suppresses nonessential delivery. Opening the application
//! resets the absence allowance without erasing cooldown or send identities.

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
    pub min_interval_secs: u64,
    pub absence_cap: u64,
}

impl Default for DeliverySettings {
    fn default() -> Self {
        Self {
            quiet_start_hour: 21,
            quiet_end_hour: 7,
            hourly_cap: 5,
            daily_cap: 20,
            max_event_age_secs: 3600,
            min_interval_secs: 21600,
            absence_cap: 2,
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

/// Unknown timezone suppresses a nonessential social push.
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
        if offset.is_none() {
            outcome("timezone_unknown");
        } else if offset.is_some_and(|offset| in_quiet_hours(offset, now, settings)) {
            outcome("quiet_hours");
        } else {
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
    Ok(
        reserve_budget_for_generation(pool, recipient, identity, settings, None).await?
            == BudgetDecision::Allowed,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDecision {
    Allowed,
    Active,
    StaleVisit,
    AllowanceExhausted,
    Cooldown { retry_after_secs: u64 },
    Capped,
}

impl BudgetDecision {
    /// Policy outcome label for a refusal. Each cause keeps its own label so an
    /// exhausted absence allowance is not reported as an hourly/daily cap.
    fn refusal_outcome(self) -> Option<&'static str> {
        match self {
            Self::Allowed => None,
            Self::Active => Some("active"),
            Self::StaleVisit => Some("stale_visit"),
            Self::AllowanceExhausted => Some("allowance_exhausted"),
            Self::Cooldown { .. } => Some("cooldown"),
            Self::Capped => Some("frequency_capped"),
        }
    }
}

/// Only direct messages bypass the nonurgent allowance and cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryClass {
    Nonurgent,
    DirectMessage,
}

impl DeliveryClass {
    fn budget_prefixes(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Nonurgent => (
                "delivery_budget",
                "delivery_generations",
                "delivery_dm_budget",
                "delivery_dm_generations",
            ),
            Self::DirectMessage => (
                "delivery_dm_budget",
                "delivery_dm_generations",
                "delivery_budget",
                "delivery_generations",
            ),
        }
    }
}

/// Atomically authorize one account interruption. Generation-aware queued work
/// is fenced by a foreground transition, including a transition after leasing.
pub async fn reserve_budget_for_generation(
    pool: &RedisPool,
    recipient: &PublicKey,
    identity: &str,
    settings: &DeliverySettings,
    generation: Option<u64>,
) -> Result<BudgetDecision> {
    reserve_budget_for_class(
        pool,
        recipient,
        identity,
        settings,
        generation,
        DeliveryClass::Nonurgent,
    )
    .await
}

/// Reserve a class-specific identity while sharing hourly/daily bounds. Direct
/// messages never spend nonurgent allowance or advance its cooldown.
pub async fn reserve_budget_for_class(
    pool: &RedisPool,
    recipient: &PublicKey,
    identity: &str,
    settings: &DeliverySettings,
    generation: Option<u64>,
    class: DeliveryClass,
) -> Result<BudgetDecision> {
    let (budget, generations, other_budget, other_generations) = class.budget_prefixes();
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?;
    let decision: (u64, u64) = redis::Script::new(r#"
        local now = tonumber(redis.call('TIME')[1])
        local direct_message = ARGV[8] == '1'
        for _, pair in ipairs({{KEYS[1], KEYS[4]}, {KEYS[5], KEYS[6]}}) do
          local expired = redis.call('ZRANGEBYSCORE', pair[1], '-inf', now - 86400)
          for _, id in ipairs(expired) do redis.call('HDEL', pair[2], id) end
          redis.call('ZREMRANGEBYSCORE', pair[1], '-inf', now - 86400)
        end
        redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', now)
        local generation = tonumber(redis.call('HGET', KEYS[2], 'generation') or '0')
        if ARGV[6] ~= '' and tonumber(ARGV[6]) ~= generation then return {2, 0} end
        if redis.call('ZCARD', KEYS[3]) > 0 then return {1, 0} end
        local hour_count = redis.call('ZCOUNT', KEYS[1], '(' .. (now - 3600), '+inf')
          + redis.call('ZCOUNT', KEYS[5], '(' .. (now - 3600), '+inf')
        local previous_score = redis.call('ZSCORE', KEYS[1], ARGV[1])
        if previous_score then
          local reserved_generation = tonumber(redis.call('HGET', KEYS[4], ARGV[1]) or '0')
          if reserved_generation ~= generation then return {2, 0} end
          -- Reusing a current-hour identity is free. Moving an older one
          -- into this hour must still fit the shared hourly bound.
          if tonumber(previous_score) <= now - 3600 and hour_count >= tonumber(ARGV[2]) then return {5, 0} end
          -- Nonurgent retries cannot jump a newer nonurgent notification.
          -- Refresh their attempt time so subsequent sends wait a full gap.
          if not direct_message then
            local latest = redis.call('ZREVRANGE', KEYS[1], 0, 1, 'WITHSCORES')
            for i = 1, #latest, 2 do
              if latest[i] ~= ARGV[1] then
                local remaining = tonumber(latest[i + 1]) + tonumber(ARGV[4]) - now
                if remaining > 0 then return {4, remaining} end
              end
            end
          end
          redis.call('ZADD', KEYS[1], now, ARGV[1])
          redis.call('EXPIRE', KEYS[1], 86400)
          redis.call('EXPIRE', KEYS[4], 86400)
          return {0, 0}
        end
        local reported = redis.call('HEXISTS', KEYS[2], 'activity_reported') == 1
        local used = redis.call('ZCARD', KEYS[1])
        if reported then used = tonumber(redis.call('HGET', KEYS[2], 'absence_used') or '0') end
        if not direct_message then
          if used >= tonumber(ARGV[5]) then return {3, 0} end
          local latest = redis.call('ZREVRANGE', KEYS[1], 0, 0, 'WITHSCORES')
          if #latest > 0 then
            local remaining = tonumber(latest[2]) + tonumber(ARGV[4]) - now
            if remaining > 0 then return {4, remaining} end
          end
        end
        if redis.call('ZCARD', KEYS[1]) + redis.call('ZCARD', KEYS[5]) >= tonumber(ARGV[3]) then return {5, 0} end
        if hour_count >= tonumber(ARGV[2]) then return {5, 0} end
        redis.call('ZADD', KEYS[1], now, ARGV[1])
        redis.call('EXPIRE', KEYS[1], 86400)
        redis.call('HSET', KEYS[4], ARGV[1], string.format('%.0f', generation))
        redis.call('EXPIRE', KEYS[4], 86400)
        if not direct_message and reported then
          redis.call('HSET', KEYS[2], 'absence_used', used + 1)
          redis.call('EXPIRE', KEYS[2], ARGV[7], 'NX')
        end
        return {0, 0}
    "#).key(format!("{budget}:{}", recipient.to_hex()))
        .key(format!("{}{}", crate::activity::STATE_PREFIX, recipient.to_hex()))
        .key(format!("{}{}", crate::activity::ACTIVE_PREFIX, recipient.to_hex()))
        .key(format!("{generations}:{}",recipient.to_hex()))
        .key(format!("{other_budget}:{}",recipient.to_hex()))
        .key(format!("{other_generations}:{}",recipient.to_hex()))
        .arg(identity).arg(settings.hourly_cap).arg(settings.daily_cap)
        .arg(settings.min_interval_secs).arg(settings.absence_cap)
        .arg(generation.map(|generation|generation.to_string()).unwrap_or_default())
        .arg(crate::activity::STATE_TTL_SECS)
        .arg(u8::from(class == DeliveryClass::DirectMessage))
        .invoke_async(&mut *conn).await.map_err(ServiceError::Redis)?;
    let decision = match decision {
        (0, _) => BudgetDecision::Allowed,
        (1, _) => BudgetDecision::Active,
        (2, _) => BudgetDecision::StaleVisit,
        (3, _) => BudgetDecision::AllowanceExhausted,
        (4, retry_after_secs) => BudgetDecision::Cooldown { retry_after_secs },
        _ => BudgetDecision::Capped,
    };
    if let Some(reason) = decision.refusal_outcome() {
        outcome(reason);
    }
    Ok(decision)
}

/// Revalidate an existing reservation immediately before a provider request.
/// This check never spends another slot or relaxes historical idempotency.
pub async fn validate_reservation(
    pool: &RedisPool,
    recipient: &PublicKey,
    identity: &str,
    settings: &DeliverySettings,
    generation: Option<u64>,
) -> Result<bool> {
    validate_reservation_for_class(
        pool,
        recipient,
        identity,
        settings,
        generation,
        DeliveryClass::Nonurgent,
    )
    .await
}

/// Recheck presence and visit fencing for either class. Only nonurgent work
/// rechecks and refreshes the shared nonurgent cooldown.
pub async fn validate_reservation_for_class(
    pool: &RedisPool,
    recipient: &PublicKey,
    identity: &str,
    settings: &DeliverySettings,
    generation: Option<u64>,
    class: DeliveryClass,
) -> Result<bool> {
    let (budget, generations, other_budget, _) = class.budget_prefixes();
    let mut conn = pool
        .get()
        .await
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    redis::Script::new(
        r#"
        local now = tonumber(redis.call('TIME')[1])
        redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', now)
        if redis.call('ZCARD', KEYS[3]) > 0 then return 0 end
        local generation = tonumber(redis.call('HGET', KEYS[2], 'generation') or '0')
        if ARGV[2] ~= '' and tonumber(ARGV[2]) ~= generation then return 0 end
        local score = redis.call('ZSCORE', KEYS[1], ARGV[1])
        if not score or tonumber(score) <= now - 86400 then return 0 end
        if tonumber(score) <= now - 3600 then
          local hour_count = redis.call('ZCOUNT', KEYS[1], '(' .. (now - 3600), '+inf')
            + redis.call('ZCOUNT', KEYS[5], '(' .. (now - 3600), '+inf')
          if hour_count >= tonumber(ARGV[5]) then return 0 end
        end
        local reserved_generation = tonumber(redis.call('HGET', KEYS[4], ARGV[1]) or '0')
        if generation ~= reserved_generation then return 0 end
        if ARGV[4] ~= '1' then
          local latest = redis.call('ZREVRANGE', KEYS[1], 0, 1, 'WITHSCORES')
          for i = 1, #latest, 2 do
            if latest[i] ~= ARGV[1] and tonumber(latest[i + 1]) + tonumber(ARGV[3]) > now then
              return 0
            end
          end
        end
        redis.call('ZADD', KEYS[1], now, ARGV[1])
        redis.call('EXPIRE', KEYS[1], 86400)
        redis.call('EXPIRE', KEYS[4], 86400)
        return 1
    "#,
    )
    .key(format!("{budget}:{}", recipient.to_hex()))
    .key(format!(
        "{}{}",
        crate::activity::STATE_PREFIX,
        recipient.to_hex()
    ))
    .key(format!(
        "{}{}",
        crate::activity::ACTIVE_PREFIX,
        recipient.to_hex()
    ))
    .key(format!("{generations}:{}", recipient.to_hex()))
    .key(format!("{other_budget}:{}", recipient.to_hex()))
    .arg(identity)
    .arg(
        generation
            .map(|generation| generation.to_string())
            .unwrap_or_default(),
    )
    .arg(settings.min_interval_secs)
    .arg(u8::from(class == DeliveryClass::DirectMessage))
    .arg(settings.hourly_cap)
    .invoke_async(&mut *conn)
    .await
    .map_err(ServiceError::Redis)
}

/// Keep provider storage short so offline devices do not replay old activity.
/// Zero TTL asks both providers to deliver immediately or discard the push;
/// this does not create a durable inbox entry. This also prevents a push accepted
/// just before quiet hours from waiting at the provider overnight.
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

#[cfg(test)]
mod tests {
    use super::BudgetDecision;

    #[test]
    fn each_budget_refusal_reports_its_own_policy_outcome() {
        assert_eq!(BudgetDecision::Allowed.refusal_outcome(), None);
        let refusals = [
            (BudgetDecision::Active, "active"),
            (BudgetDecision::StaleVisit, "stale_visit"),
            (BudgetDecision::AllowanceExhausted, "allowance_exhausted"),
            (
                BudgetDecision::Cooldown {
                    retry_after_secs: 60,
                },
                "cooldown",
            ),
            (BudgetDecision::Capped, "frequency_capped"),
        ];
        for (decision, label) in refusals {
            assert_eq!(decision.refusal_outcome(), Some(label), "{decision:?}");
        }
    }
}
