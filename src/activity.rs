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
    let reset: bool = redis::Script::new(
        r#"
        local previous_sequence = tonumber(redis.call('HGET', KEYS[1], 'sequence') or '0')
        if tonumber(ARGV[2]) <= previous_sequence then return 0 end
        local previous_visit = tonumber(redis.call('HGET', KEYS[1], 'visit') or '0')
        if tonumber(ARGV[6]) < previous_visit then return 0 end
        local now = tonumber(redis.call('TIME')[1])
        redis.call('HSET', KEYS[1], 'sequence', ARGV[2], 'state', ARGV[3], 'visit', ARGV[6])
        redis.call('EXPIRE', KEYS[1], ARGV[5])
        redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
        local reset = 0
        if tonumber(ARGV[6]) > previous_visit then
          redis.call('HINCRBY', KEYS[3], 'generation', 1)
          redis.call('HSET', KEYS[3], 'absence_used', 0)
          reset = 1
        end
        if ARGV[3] == 'foreground' then
          redis.call('ZADD', KEYS[2], now + tonumber(ARGV[4]), ARGV[1])
          redis.call('EXPIRE', KEYS[2], ARGV[4])
        else
          redis.call('ZREM', KEYS[2], ARGV[1])
        end
        redis.call('HSET', KEYS[3], 'last_activity_at', now)
        return reset
    "#,
    )
    .key(format!(
        "notification_activity_session:{owner}:{}",
        update.session_id
    ))
    .key(format!("{ACTIVE_PREFIX}{owner}"))
    .key(format!("{STATE_PREFIX}{owner}"))
    .arg(update.session_id.to_string())
    .arg(update.sequence)
    .arg(match update.state {
        ActivityState::Foreground => "foreground",
        ActivityState::Background => "background",
    })
    .arg(PRESENCE_TTL_SECS)
    .arg(SESSION_TTL_SECS)
    .arg(update.visit)
    .invoke_async(&mut *conn)
    .await
    .map_err(ServiceError::Redis)?;
    metrics::counter!("push_activity_total", "outcome" => if reset { "visit" } else { "renewed_or_stale" }).increment(1);
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
/// Retain at least a week, and longer when the work's expiry requires it.
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
        if not generation then
          generation = redis.call('HGET', KEYS[2], 'generation') or '0'
          redis.call('SET', KEYS[1], generation, 'EX', ARGV[1])
        elseif redis.call('TTL', KEYS[1]) < tonumber(ARGV[1]) then
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
    .arg(retention_secs.max(SESSION_TTL_SECS))
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
