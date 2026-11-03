//! Account-scoped foreground presence and ordered visit transitions.
//! Activity changes push eligibility only; it never acknowledges inbox records.

use nostr_sdk::PublicKey;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    error::{Result, ServiceError},
    redis_store::RedisPool,
};

pub const STATE_PREFIX: &str = "notification_activity:";
pub const ACTIVE_PREFIX: &str = "notification_active:";
pub const PRESENCE_TTL_SECS: u64 = 180;
const SESSION_TTL_SECS: u64 = 604800;
/// Bound retained sessions while allowing multiple devices and process restarts.
pub const MAX_SESSIONS_PER_ACCOUNT: u64 = 128;
pub const STATE_TTL_SECS: u64 = 90 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityState {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityUpdate {
    pub session_id: Uuid,
    pub sequence: u32,
    pub visit: u32,
    pub state: ActivityState,
}

impl ActivityUpdate {
    pub fn is_valid(&self) -> bool {
        (1..=i32::MAX as u32).contains(&self.sequence)
            && self.visit > 0
            && self.visit <= self.sequence
    }
}

/// Apply a sequence once. A heartbeat renews presence without granting quota.
/// Background and foreground messages from the same process are ordered even
/// if a network retry arrives after a later lifecycle transition.
pub async fn record_activity(
    pool: &RedisPool,
    recipient: &PublicKey,
    update: &ActivityUpdate,
) -> Result<()> {
    if !update.is_valid() {
        return Err(ServiceError::Internal("Invalid activity sequence".into()));
    }
    let owner = recipient.to_hex();
    let mut conn = pool
        .get()
        .await
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let reset: i64 = redis::Script::new(
        r#"
        if redis.call('EXISTS', KEYS[4]) == 0 then return 0 end
        local previous_sequence = tonumber(redis.call('HGET', KEYS[1], 'sequence') or '0')
        if tonumber(ARGV[2]) <= previous_sequence then return 0 end
        local previous_visit = tonumber(redis.call('HGET', KEYS[1], 'visit') or '0')
        if tonumber(ARGV[6]) < previous_visit then return 0 end
        local now = tonumber(redis.call('TIME')[1])
        redis.call('ZREMRANGEBYSCORE', KEYS[6], '-inf', now)
        if not redis.call('ZSCORE', KEYS[6], ARGV[1])
          and redis.call('ZCARD', KEYS[6]) >= tonumber(ARGV[8]) then
          return -1
        end
        redis.call('ZADD', KEYS[6], now + tonumber(ARGV[5]), ARGV[1])
        redis.call('EXPIRE', KEYS[6], ARGV[5])
        redis.call('HSET', KEYS[1], 'sequence', ARGV[2], 'state', ARGV[3], 'visit', ARGV[6])
        redis.call('EXPIRE', KEYS[1], ARGV[5])
        redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
        local reset = 0
        if tonumber(ARGV[6]) > previous_visit then
          -- This counter contains no account data and must outlive account
          -- hashes: long-lived campaign work can retain an earlier generation
          -- after the account's 90-day expiry. Seed above legacy visit counts
          -- so a rolling upgrade cannot reuse their small generation values.
          local clock = redis.call('TIME')
          redis.call('SET', KEYS[5], string.format('%.0f',
            tonumber(clock[1]) * 1000000 + tonumber(clock[2])), 'NX')
          local generation = redis.call('INCR', KEYS[5])
          redis.call('HSET', KEYS[3], 'generation', string.format('%.0f', generation))
          redis.call('HSET', KEYS[3], 'absence_used', 0)
          reset = 1
        end
        if ARGV[3] == 'foreground' then
          redis.call('ZADD', KEYS[2], now + tonumber(ARGV[4]), ARGV[1])
          redis.call('EXPIRE', KEYS[2], ARGV[4])
        else
          redis.call('ZREM', KEYS[2], ARGV[1])
        end
        redis.call('HSET', KEYS[3], 'activity_reported', 1)
        redis.call('HDEL', KEYS[3], 'last_activity_at')
        redis.call('EXPIRE', KEYS[3], ARGV[7])
        return reset
    "#,
    )
    .key(format!(
        "notification_activity_session:{owner}:{}",
        update.session_id
    ))
    .key(format!("{ACTIVE_PREFIX}{owner}"))
    .key(format!("{STATE_PREFIX}{owner}"))
    .key(format!("user_tokens:{owner}"))
    .key("notification_visit_sequence")
    .key(format!("notification_activity_sessions:{owner}"))
    .arg(update.session_id.to_string())
    .arg(update.sequence)
    .arg(match update.state {
        ActivityState::Foreground => "foreground",
        ActivityState::Background => "background",
    })
    .arg(PRESENCE_TTL_SECS)
    .arg(SESSION_TTL_SECS)
    .arg(update.visit)
    .arg(STATE_TTL_SECS)
    .arg(MAX_SESSIONS_PER_ACCOUNT)
    .invoke_async(&mut *conn)
    .await
    .map_err(ServiceError::Redis)?;
    if reset == -1 {
        metrics::counter!("push_activity_total", "outcome" => "session_limit").increment(1);
        return Err(ServiceError::ActivitySessionLimit);
    }
    metrics::counter!("push_activity_total", "outcome" => if reset == 1 { "visit" } else { "renewed_or_stale" }).increment(1);
    Ok(())
}

/// Current account visit generation; uninstrumented legacy clients start at 0.
pub async fn current_generation(pool: &RedisPool, recipient: &PublicKey) -> Result<u64> {
    let mut conn = pool
        .get()
        .await
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let generation: Option<u64> = redis::cmd("HGET")
        .arg(format!("{STATE_PREFIX}{}", recipient.to_hex()))
        .arg("generation")
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(generation.unwrap_or(0))
}

/// Bind a recipient/work identity before fallible preflight. A retry must retain
/// its first generation even when no interruption slot was reserved yet.
/// Retain for the caller's retry horizon, with a minimum TTL of one second.
/// Public activity for an account without registered devices creates no marker.
pub async fn generation_for_work(
    pool: &RedisPool,
    recipient: &PublicKey,
    identity: &str,
    retention_secs: u64,
) -> Result<u64> {
    let mut conn = pool
        .get()
        .await
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    redis::Script::new(
        r#"
        local generation = redis.call('GET', KEYS[1])
        local registered = redis.call('EXISTS', KEYS[3]) == 1
        local current = redis.call('HGET', KEYS[2], 'generation')
        if registered and not current then
          -- Bind pre-visit work to a real epoch too. Otherwise an old work
          -- marker bound to legacy generation 0 could match missing state
          -- again after the 90-day retention window.
          local clock = redis.call('TIME')
          redis.call('SET', KEYS[4], string.format('%.0f',
            tonumber(clock[1]) * 1000000 + tonumber(clock[2])), 'NX')
          current = string.format('%.0f', redis.call('INCR', KEYS[4]))
          redis.call('HSET', KEYS[2], 'generation', current)
          redis.call('EXPIRE', KEYS[2], ARGV[2], 'NX')
        end
        if not generation then
          generation = current or '0'
          if registered then redis.call('SET', KEYS[1], generation, 'EX', ARGV[1]) end
        elseif registered and redis.call('TTL', KEYS[1]) < tonumber(ARGV[1]) then
          redis.call('EXPIRE', KEYS[1], ARGV[1])
        end
        return tonumber(generation)
    "#,
    )
    .key(format!(
        "notification_work_generation:{}:{}",
        recipient.to_hex(),
        blake3::hash(identity.as_bytes()).to_hex()
    ))
    .key(format!("{STATE_PREFIX}{}", recipient.to_hex()))
    .key(format!("user_tokens:{}", recipient.to_hex()))
    .key("notification_visit_sequence")
    .arg(retention_secs.max(1))
    .arg(STATE_TTL_SECS)
    .invoke_async(&mut *conn)
    .await
    .map_err(ServiceError::Redis)
}

/// Presence is account-wide: a background device cannot clear another lease.
pub async fn is_active(pool: &RedisPool, recipient: &PublicKey) -> Result<bool> {
    let mut conn = pool
        .get()
        .await
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    redis::Script::new(
        r#"
        redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', redis.call('TIME')[1])
        return redis.call('ZCARD', KEYS[1]) > 0
    "#,
    )
    .key(format!("{ACTIVE_PREFIX}{}", recipient.to_hex()))
    .invoke_async(&mut *conn)
    .await
    .map_err(ServiceError::Redis)
}
