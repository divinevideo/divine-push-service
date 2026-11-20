//! Redis store for Live Activities and their due schedule.
//!
//! Kept entirely apart from device registration: nothing here reads or writes
//! `user_tokens:*`, `token_to_pubkey` or any other registration key, so ending,
//! finishing or failing an activity can never deregister a device.
//!
//! Keys (id = blake3 hex of `{owner}:{activity_token}`, so each signer's records
//! live in its own namespace and no signer can block or hijack another's activity):
//! - `live_activity:{id}`: hash of one activity. Live until `window_ends_at` + 2100 s.
//!   A tombstone (`state = ended`, both tokens dropped) lives until `window_ends_at` + 1800 s.
//! - `live_activity_owner:{owner}`: set of the owner's activity ids, for the per-owner cap.
//! - `live_activity_device:{owner}:{blake3(fcm_token)}`: the device's current activity id.
//! - `live_activity_jobs`: sorted set of active ids scored by due time in Unix ms.
//! - `live_activity_token_owner:{blake3(activity_token)}`: the owner pubkey the
//!   token is bound to. The first owner to store an activity for a token holds it
//!   until the activity hash's horizon, so nobody who learns the token can attach
//!   their own counts to it. Ends do not need it: they stay in the signer's namespace.
//!
//! Every write is one Lua script, so concurrent replicas cannot interleave. The
//! deployment uses a single Redis primary, so scripts may touch keys they derive.

use crate::error::{Result, ServiceError};
use crate::live_activity::payload::{ActivityState, LiveActivityRegistration};
use crate::redis_store::RedisPool;
use redis::Script;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

const ACTIVITY_PREFIX: &str = "live_activity:";
const OWNER_PREFIX: &str = "live_activity_owner:";
const DEVICE_PREFIX: &str = "live_activity_device:";
const JOBS_ZSET: &str = "live_activity_jobs";
const TOKEN_OWNER_PREFIX: &str = "live_activity_token_owner:";

/// Seconds a tombstone outlives the window: matches the end event's dismissal date.
const TOMBSTONE_GRACE_SECS: u64 = 1800;
/// Extra seconds a live hash outlives its tombstone horizon, so a worker can still end it.
const LIVE_EXTRA_SECS: u64 = 300;

/// An active activity claimed from the schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredActivity {
    /// blake3 hex of `{owner}:{activity_token}`.
    pub id: String,
    /// Hex pubkey that registered the activity.
    pub owner: String,
    /// Firebase registration token of the device. Never log it whole.
    pub fcm_token: String,
    /// The stored registration; `state` is always `Active` for a claimed activity.
    pub registration: LiveActivityRegistration,
    /// Server time (Unix secs) the activity was first stored; carried over on token rotation.
    pub first_seen_at: u64,
    /// Consecutive successful stats checks that found none of the post's ids.
    pub absent_checks: u32,
    /// Content state of the last successful send, if any.
    pub last_content: Option<serde_json::Value>,
}

/// Result of storing an `active` registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// A new activity was stored and scheduled.
    Created,
    /// The same activity token was updated; its schedule is unchanged.
    Updated,
    /// The device's previous activity was finished. When the new token is for the
    /// same post (an Apple token rotation), its schedule and send state were carried over.
    Superseded { previous_id: String },
    /// An event that is not newer than the stored one (or than the device's current
    /// activity), by (`created_at`, lower id); or a window past its dismissal horizon.
    Stale,
    /// The activity was ended or finished; it never starts again.
    Tombstoned,
    /// The owner already holds `max_per_owner` active activities. Nothing stored.
    OverOwnerCap,
    /// The service already tracks `max_total` active activities. Nothing stored.
    OverTotalCap,
    /// The activity token is bound to another owner's live or recent activity.
    /// Nothing stored.
    TokenOwnedElsewhere,
}

/// Result of an `ended` registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndOutcome {
    /// A live activity was ended and unscheduled.
    Ended,
    /// The activity was never seen; a tombstone now prevents it from starting.
    TombstonedUnseen,
    /// The activity was already ended, or the event is older than its last update.
    Stale,
}

/// Lua shared by every script. `tombstone` keeps owner, order stamp and window,
/// drops both tokens, unschedules the id and clears its owner and device entries.
const PRELUDE: &str = r#"
local PREFIX = 'live_activity:'
local OWNER_PREFIX = 'live_activity_owner:'
local DEVICE_PREFIX = 'live_activity_device:'
local JOBS = 'live_activity_jobs'

local function is_live(id)
  return redis.call('HGET', PREFIX .. id, 'state') == 'active'
end

-- Order stamps are a 20-digit created_at plus event id; the lower id wins a tie.
local function newer(at, eid, last_at, last_id)
  if not last_at then return true end
  if at ~= last_at then return at > last_at end
  return eid < (last_id or '')
end

local function tombstone(id, at, eid)
  local key = PREFIX .. id
  local f = redis.call('HMGET', key, 'owner', 'fcm_hash', 'window_ends_at')
  redis.call('ZREM', JOBS, id)
  if f[1] then
    redis.call('SREM', OWNER_PREFIX .. f[1], id)
    if f[2] then
      local dkey = DEVICE_PREFIX .. f[1] .. ':' .. f[2]
      if redis.call('GET', dkey) == id then redis.call('DEL', dkey) end
    end
  end
  redis.call('HDEL', key, 'fcm_token', 'activity_token', 'fcm_hash')
  redis.call('HSET', key, 'state', 'ended')
  if at then redis.call('HSET', key, 'last_at', at, 'last_id', eid) end
  if f[3] then redis.call('EXPIREAT', key, tonumber(f[3]) + 1800) end
end
"#;

static UPSERT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(&format!(
        "{PRELUDE}{}",
        r#"
local key, okey, dkey, tkey = KEYS[1], KEYS[2], KEYS[3], KEYS[5]
local id, owner, fcm, fcm_hash = ARGV[1], ARGV[2], ARGV[3], ARGV[4]
local reg = cjson.decode(ARGV[5])
local at, eid, due = ARGV[6], ARGV[7], ARGV[8]
local max_owner, max_total = tonumber(ARGV[9]), tonumber(ARGV[10])
local now, exat = tonumber(ARGV[11]), tonumber(ARGV[12])

-- Union keeping order (stored first), capped at 10.
local function merge(stored_json, incoming)
  local out, seen = {}, {}
  local stored = {}
  if stored_json then stored = cjson.decode(stored_json) end
  for _, list in ipairs({stored, incoming}) do
    for _, v in ipairs(list) do
      if not seen[v] and #out < 10 then
        seen[v] = true
        out[#out + 1] = v
      end
    end
  end
  return cjson.encode(out)
end

-- First owner wins the token until the activity's horizon; refreshed like the hash.
local bound = redis.call('GET', tkey)
if bound and bound ~= owner then return {8, ''} end

local function attach()
  redis.call('SET', tkey, owner, 'EXAT', exat)
  redis.call('SET', dkey, id, 'EXAT', exat)
  redis.call('SADD', okey, id)
  local ttl = redis.call('TTL', okey)
  if ttl < 0 or now + ttl < exat then redis.call('EXPIREAT', okey, exat) end
end

-- The device's current activity, when live and not this one, must be older to be replaced.
local function displaced_or_stale()
  local cur = redis.call('GET', dkey)
  if not cur or cur == id or not is_live(cur) then return false, false end
  local c = redis.call('HMGET', PREFIX .. cur, 'last_at', 'last_id')
  if not newer(at, eid, c[1], c[2]) then return false, true end
  return cur, false
end

if redis.call('EXISTS', key) == 1 then
  local s = redis.call('HMGET', key, 'state', 'last_at', 'last_id', 'post_event_ids', 'fcm_hash')
  if s[1] ~= 'active' then return {4, ''} end
  if not newer(at, eid, s[2], s[3]) then return {3, ''} end
  local displaced, stale = displaced_or_stale()
  if stale then return {3, ''} end
  if displaced then tombstone(displaced) end
  if s[5] and s[5] ~= fcm_hash then
    local old = DEVICE_PREFIX .. owner .. ':' .. s[5]
    if redis.call('GET', old) == id then redis.call('DEL', old) end
  end
  redis.call('HSET', key,
    'fcm_token', fcm, 'fcm_hash', fcm_hash,
    'post_coordinate', reg.post_coordinate,
    'post_event_ids', merge(s[4], reg.post_event_ids),
    'posted_at', reg.posted_at, 'window_ends_at', reg.window_ends_at,
    'show_first_run_note', reg.show_first_run_note,
    'last_at', at, 'last_id', eid)
  redis.call('EXPIREAT', key, exat)
  attach()
  if displaced then return {2, displaced} end
  return {1, ''}
end

local prev, stale = displaced_or_stale()
if stale then return {3, ''} end

-- Count live activities, pruning ids whose hash expired or ended. The activity
-- being superseded does not count, so a replacement never trips a cap.
local owned = 0
for _, m in ipairs(redis.call('SMEMBERS', okey)) do
  if not is_live(m) then
    redis.call('SREM', okey, m)
  elseif m ~= prev then
    owned = owned + 1
  end
end
if owned >= max_owner then return {5, ''} end
local total = redis.call('ZCARD', JOBS)
if prev and redis.call('ZSCORE', JOBS, prev) then total = total - 1 end
if total >= max_total then return {6, ''} end

local fields = {
  'owner', owner, 'fcm_token', fcm, 'fcm_hash', fcm_hash,
  'activity_token', reg.activity_token, 'post_coordinate', reg.post_coordinate,
  'posted_at', reg.posted_at, 'window_ends_at', reg.window_ends_at,
  'show_first_run_note', reg.show_first_run_note,
  'last_at', at, 'last_id', eid, 'state', 'active',
}
local ids, score = merge(false, reg.post_event_ids), due
local first_seen, absent = ARGV[11], '0'

if prev then
  local p = redis.call('HMGET', PREFIX .. prev, 'post_coordinate', 'first_seen_at',
    'absent_checks', 'last_content', 'last_sent_at', 'post_event_ids')
  if p[1] == reg.post_coordinate then
    -- Apple token rotation: the same post continues under the new token.
    first_seen = p[2] or first_seen
    absent = p[3] or absent
    if p[4] then fields[#fields + 1] = 'last_content'; fields[#fields + 1] = p[4] end
    if p[5] then fields[#fields + 1] = 'last_sent_at'; fields[#fields + 1] = p[5] end
    ids = merge(p[6], reg.post_event_ids)
    score = redis.call('ZSCORE', JOBS, prev) or due
  end
  tombstone(prev)
end

for _, v in ipairs({'post_event_ids', ids, 'first_seen_at', first_seen, 'absent_checks', absent}) do
  fields[#fields + 1] = v
end
redis.call('HSET', key, unpack(fields))
redis.call('EXPIREAT', key, exat)
redis.call('ZADD', JOBS, score, id)
attach()
if prev then return {2, prev} end
return {0, ''}
"#
    ))
});

static END: LazyLock<Script> = LazyLock::new(|| {
    Script::new(&format!(
        "{PRELUDE}{}",
        r#"
local key = KEYS[1]
local id, owner, at, eid = ARGV[1], ARGV[2], ARGV[3], ARGV[4]
if redis.call('EXISTS', key) == 1 then
  local s = redis.call('HMGET', key, 'state', 'last_at')
  if s[1] ~= 'active' then return 2 end
  -- Only a strictly older end is stale: like a deregistration, an end wins a
  -- same-second tie, so the person's end is never lost to an id comparison.
  if s[2] and at < s[2] then return 2 end
  tombstone(id, at, eid)
  return 0
end
redis.call('HSET', key, 'owner', owner, 'last_at', at, 'last_id', eid,
  'state', 'ended', 'window_ends_at', ARGV[5])
redis.call('EXPIREAT', key, tonumber(ARGV[5]) + 1800)
return 1
"#
    ))
});

static CLAIM: LazyLock<Script> = LazyLock::new(|| {
    Script::new(&format!(
        "{PRELUDE}{}",
        r#"
local out = {}
for _, id in ipairs(redis.call('ZRANGEBYSCORE', JOBS, '-inf', ARGV[1], 'LIMIT', 0, ARGV[3])) do
  if is_live(id) then
    redis.call('ZADD', JOBS, ARGV[2], id)
    out[#out + 1] = {id, redis.call('HGETALL', PREFIX .. id)}
  else
    redis.call('ZREM', JOBS, id)
  end
end
return out
"#
    ))
});

static RESCHEDULE: LazyLock<Script> = LazyLock::new(|| {
    Script::new(&format!(
        "{PRELUDE}{}",
        r#"
if is_live(ARGV[1]) then
  redis.call('ZADD', JOBS, ARGV[2], ARGV[1])
  return 1
end
redis.call('ZREM', JOBS, ARGV[1])
return 0
"#
    ))
});

static SET_IF_LIVE: LazyLock<Script> = LazyLock::new(|| {
    Script::new(&format!(
        "{PRELUDE}{}",
        r#"
if not is_live(ARGV[1]) then return 0 end
local fields = {}
for i = 2, #ARGV do fields[#fields + 1] = ARGV[i] end
redis.call('HSET', PREFIX .. ARGV[1], unpack(fields))
return 1
"#
    ))
});

static FINISH: LazyLock<Script> = LazyLock::new(|| {
    Script::new(&format!(
        "{PRELUDE}{}",
        r#"
if is_live(ARGV[1]) then
  tombstone(ARGV[1])
else
  redis.call('ZREM', JOBS, ARGV[1])
end
return 1
"#
    ))
});

fn now_secs() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| ServiceError::Internal(format!("system clock before Unix epoch: {e}")))
}

fn blake3_hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

/// Activity id: blake3 hex of `{owner}:{activity_token}`. Scoping by owner means a
/// signer can only ever address its own records, and the key never holds the token.
pub fn activity_id(owner: &str, activity_token: &str) -> String {
    blake3_hex(&format!("{owner}:{activity_token}"))
}

/// Fixed-width decimal so Lua compares the full u64 order as strings.
fn order_stamp(created_at: u64) -> String {
    format!("{created_at:020}")
}

async fn conn(
    pool: &RedisPool,
) -> Result<bb8_redis::bb8::PooledConnection<'_, crate::redis_connection::PrimaryConnectionManager>>
{
    pool.get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))
}

/// Store or update an `active` registration and schedule it.
///
/// `first_due_ms` is used only when a new schedule is created. A registration
/// whose `window_ends_at` + 1800 s has passed is `Stale`. Caps count live
/// activities: per owner from `live_activity_owner:{owner}`, in total from
/// `ZCARD live_activity_jobs`. A superseded activity (same owner and device)
/// frees its slot for its replacement.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_active(
    pool: &RedisPool,
    owner: &str,
    fcm_token: &str,
    reg: &LiveActivityRegistration,
    event_at: u64,
    event_id: &str,
    first_due_ms: u64,
    max_per_owner: usize,
    max_total: usize,
) -> Result<UpsertOutcome> {
    let id = activity_id(owner, &reg.activity_token);
    let fcm_hash = blake3_hex(fcm_token);
    // Numbers travel as strings so Lua never rounds them through a double.
    let reg_json = serde_json::json!({
        "activity_token": reg.activity_token,
        "post_coordinate": reg.post_coordinate,
        "post_event_ids": reg.post_event_ids,
        "posted_at": reg.posted_at.to_string(),
        "window_ends_at": reg.window_ends_at.to_string(),
        "show_first_run_note": if reg.show_first_run_note { "1" } else { "0" },
    });
    let now = now_secs()?;
    // Past its dismissal horizon the activity is over, and any tombstone for it has
    // expired: storing it would write keys with a past EXAT (deleting the owner set).
    if reg.window_ends_at.saturating_add(TOMBSTONE_GRACE_SECS) <= now {
        return Ok(UpsertOutcome::Stale);
    }
    let exat = reg
        .window_ends_at
        .saturating_add(TOMBSTONE_GRACE_SECS + LIVE_EXTRA_SECS);
    let mut conn = conn(pool).await?;
    let (code, other): (i64, String) = UPSERT
        .key(format!("{ACTIVITY_PREFIX}{id}"))
        .key(format!("{OWNER_PREFIX}{owner}"))
        .key(format!("{DEVICE_PREFIX}{owner}:{fcm_hash}"))
        .key(JOBS_ZSET)
        .key(format!(
            "{TOKEN_OWNER_PREFIX}{}",
            blake3_hex(&reg.activity_token)
        ))
        .arg(&id)
        .arg(owner)
        .arg(fcm_token)
        .arg(&fcm_hash)
        .arg(reg_json.to_string())
        .arg(order_stamp(event_at))
        .arg(event_id)
        .arg(first_due_ms.to_string())
        .arg(max_per_owner)
        .arg(max_total)
        .arg(now)
        .arg(exat)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(match code {
        0 => UpsertOutcome::Created,
        1 => UpsertOutcome::Updated,
        2 => UpsertOutcome::Superseded { previous_id: other },
        3 => UpsertOutcome::Stale,
        4 => UpsertOutcome::Tombstoned,
        5 => UpsertOutcome::OverOwnerCap,
        6 => UpsertOutcome::OverTotalCap,
        8 => UpsertOutcome::TokenOwnedElsewhere,
        other => {
            return Err(ServiceError::Internal(format!(
                "live activity upsert returned unknown code {other}"
            )))
        }
    })
}

/// Apply an `ended` registration, matched by activity token and owner.
///
/// For an activity never seen, writes a tombstone (owner and order stamp, no
/// tokens) expiring at `reg.window_ends_at` + 1800 s, so a later or replayed
/// `active` never starts it.
pub async fn end_by_token(
    pool: &RedisPool,
    owner: &str,
    reg: &LiveActivityRegistration,
    event_at: u64,
    event_id: &str,
) -> Result<EndOutcome> {
    let id = activity_id(owner, &reg.activity_token);
    let mut conn = conn(pool).await?;
    let code: i64 = END
        .key(format!("{ACTIVITY_PREFIX}{id}"))
        .arg(&id)
        .arg(owner)
        .arg(order_stamp(event_at))
        .arg(event_id)
        .arg(reg.window_ends_at)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(match code {
        0 => EndOutcome::Ended,
        1 => EndOutcome::TombstonedUnseen,
        _ => EndOutcome::Stale,
    })
}

fn field<'a>(h: &'a HashMap<String, String>, name: &str, id: &str) -> Result<&'a str> {
    h.get(name).map(String::as_str).ok_or_else(|| {
        ServiceError::Internal(format!("live activity {id} is missing field {name}"))
    })
}

fn number<T: std::str::FromStr>(h: &HashMap<String, String>, name: &str, id: &str) -> Result<T> {
    field(h, name, id)?
        .parse()
        .map_err(|_| ServiceError::Internal(format!("live activity {id} has a bad {name}")))
}

fn parse_stored(id: String, h: HashMap<String, String>) -> Result<StoredActivity> {
    let post_event_ids: Vec<String> = serde_json::from_str(field(&h, "post_event_ids", &id)?)
        .map_err(|_| {
            ServiceError::Internal(format!("live activity {id} has bad post_event_ids"))
        })?;
    let registration = LiveActivityRegistration {
        activity_token: field(&h, "activity_token", &id)?.to_string(),
        post_coordinate: field(&h, "post_coordinate", &id)?.to_string(),
        post_event_ids,
        posted_at: number(&h, "posted_at", &id)?,
        window_ends_at: number(&h, "window_ends_at", &id)?,
        show_first_run_note: field(&h, "show_first_run_note", &id)? == "1",
        state: ActivityState::Active,
    };
    Ok(StoredActivity {
        owner: field(&h, "owner", &id)?.to_string(),
        fcm_token: field(&h, "fcm_token", &id)?.to_string(),
        registration,
        first_seen_at: number(&h, "first_seen_at", &id)?,
        absent_checks: number(&h, "absent_checks", &id)?,
        last_content: h
            .get("last_content")
            .and_then(|s| serde_json::from_str(s).ok()),
        id,
    })
}

/// Claim up to `limit` due activities, leasing each by rescoring it to `now_ms + lease_ms`.
///
/// Members whose hash is gone or tombstoned are dropped from the schedule. A hash
/// that cannot be parsed is left out of the batch and finished, so it is not claimed
/// again every lease; a failure to finish it is logged and the batch still returns.
pub async fn claim_due(
    pool: &RedisPool,
    now_ms: u64,
    lease_ms: u64,
    limit: usize,
) -> Result<Vec<StoredActivity>> {
    let rows: Vec<(String, HashMap<String, String>)> = {
        let mut conn = conn(pool).await?;
        CLAIM
            .key(JOBS_ZSET)
            .arg(now_ms.to_string())
            .arg(now_ms.saturating_add(lease_ms).to_string())
            .arg(limit)
            .invoke_async(&mut *conn)
            .await
            .map_err(ServiceError::Redis)?
    };
    let mut claimed = Vec::with_capacity(rows.len());
    for (id, hash) in rows {
        match parse_stored(id.clone(), hash) {
            Ok(stored) => claimed.push(stored),
            Err(e) => {
                tracing::warn!(activity_id = %id, error = %e, "finishing unreadable live activity");
                // The rest of the batch is already leased: never drop it for this row.
                if let Err(e) = finish(pool, &id).await {
                    tracing::warn!(activity_id = %id, error = %e, "could not finish unreadable live activity");
                }
            }
        }
    }
    Ok(claimed)
}

/// Schedule a live activity's next run. Returns false, and leaves it unscheduled,
/// when the hash is gone or tombstoned, so a slow worker cannot revive a finished job.
pub async fn reschedule(pool: &RedisPool, id: &str, due_ms: u64) -> Result<bool> {
    let mut conn = conn(pool).await?;
    let revived: i64 = RESCHEDULE
        .key(JOBS_ZSET)
        .key(format!("{ACTIVITY_PREFIX}{id}"))
        .arg(id)
        .arg(due_ms.to_string())
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(revived == 1)
}

async fn set_if_live(pool: &RedisPool, id: &str, fields: &[(&str, String)]) -> Result<()> {
    let mut conn = conn(pool).await?;
    let mut invocation = SET_IF_LIVE.key(format!("{ACTIVITY_PREFIX}{id}"));
    invocation.arg(id);
    for (name, value) in fields {
        invocation.arg(*name).arg(value);
    }
    let _: i64 = invocation
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(())
}

/// Record the content state of a successful send. No-op unless the activity is live.
pub async fn record_sent(
    pool: &RedisPool,
    id: &str,
    content_json: &str,
    sent_at: u64,
) -> Result<()> {
    set_if_live(
        pool,
        id,
        &[
            ("last_content", content_json.to_string()),
            ("last_sent_at", sent_at.to_string()),
        ],
    )
    .await
}

/// Store the consecutive-absence count. No-op unless the activity is live.
pub async fn set_absent_checks(pool: &RedisPool, id: &str, count: u32) -> Result<()> {
    set_if_live(pool, id, &[("absent_checks", count.to_string())]).await
}

/// True when the activity exists and is not tombstoned. Check right before any end send.
pub async fn is_live(pool: &RedisPool, id: &str) -> Result<bool> {
    let mut conn = conn(pool).await?;
    let state: Option<String> = redis::cmd("HGET")
        .arg(format!("{ACTIVITY_PREFIX}{id}"))
        .arg("state")
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(state.as_deref() == Some("active"))
}

/// Finish an activity: tombstone it (both tokens dropped) and unschedule it.
/// Harmless for an unknown or already finished id.
pub async fn finish(pool: &RedisPool, id: &str) -> Result<()> {
    let mut conn = conn(pool).await?;
    let _: i64 = FINISH
        .key(format!("{ACTIVITY_PREFIX}{id}"))
        .key(JOBS_ZSET)
        .arg(id)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(())
}

/// Age in seconds of the oldest due activity, or `None` when nothing is due.
pub async fn oldest_due_age_secs(pool: &RedisPool, now_ms: u64) -> Result<Option<u64>> {
    let mut conn = conn(pool).await?;
    let head: Vec<(String, u64)> = redis::cmd("ZRANGEBYSCORE")
        .arg(JOBS_ZSET)
        .arg("-inf")
        .arg(now_ms)
        .arg("LIMIT")
        .arg(0)
        .arg(1)
        .arg("WITHSCORES")
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(head
        .first()
        .map(|(_, due)| now_ms.saturating_sub(*due) / 1000))
}

/// Number of scheduled (active) activities, the same on every replica.
pub async fn active_count(pool: &RedisPool) -> Result<u64> {
    let mut conn = conn(pool).await?;
    redis::cmd("ZCARD")
        .arg(JOBS_ZSET)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)
}
