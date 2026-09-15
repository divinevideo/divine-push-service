//! Like and repost coalescing.
//!
//! One popular post must not buzz its author once per like. The first few
//! interactions in a two-hour bucket still send immediately; the rest collect
//! in a per-`(recipient, type, target, bucket)` group and flush once as a
//! summary. Bucket boundaries and deadlines come from the Redis server clock
//! (`TIME`) inside the ingest script, so every replica agrees and no client
//! clock participates.
//!
//! The design record is `docs/plans/like-repost-coalescing.md`. Two properties
//! carry the correctness here:
//!
//! - A group is immutable once its bucket deadline passes. An event arriving
//!   after the deadline computes the next bucket and can never join a group
//!   that is being flushed, so the flush snapshot cannot miss late work.
//! - Ingest decides and records everything in one Lua script. A replay reads
//!   the stored disposition and reproduces the original decision and collapse
//!   id; it never double-counts.
//!
//! Comments, mentions, and new-post ("bell") notifications deliberately do not
//! coalesce: they have no reliably retrievable durable inbox row, so collapsing
//! one can lose it permanently.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::{PublicKey, ToBech32};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::ServiceSettings;
use crate::error::{Result, ServiceError};
use crate::fcm_sender::FcmError;
use crate::models::FcmPayload;
use crate::preferences::{self, NotificationType};
use crate::redis_store::{self, RedisPool};
use crate::state::AppState;

/// Key prefixes for the coalescing schema. Kept as prefixes rather than built
/// keys because the ingest and claim scripts compose the full keys inside the
/// script, after the bucket is derived from server time.
pub const GROUP_PREFIX: &str = "coalesce:g:";
pub const HLL_PREFIX: &str = "coalesce:hll:";
pub const DISP_PREFIX: &str = "coalesce:disp:";
pub const DUE_KEY: &str = "coalesce:due";
pub const LEASES_KEY: &str = "coalesce:leases";
pub const THROTTLE_PREFIX: &str = "coalesce:throttle:";
pub const EMITTED_PREFIX: &str = "coalesce:emitted:";

/// Expired leases reconciled in one claim call. Bounded so one claim cannot
/// stall Redis behind an unbounded recovery pass.
const MAX_RECOVERY_PER_CLAIM: usize = 64;
/// Dangling due members removed in one claim call, bounded for the same reason.
const MAX_DANGLING_SKIPS: usize = 8;

/// Where a like/repost points. A summary can only be built for a group with a
/// stable target, so events without one keep the immediate path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalesceTarget {
    /// Redis-safe target identity: `e:{event-id-hex}` or
    /// `a:{kind:pubkey:d-tag}`.
    pub key: String,
    /// Event reference, when the trigger pointed at an event.
    pub event_id: Option<String>,
    /// Addressable reference, when the trigger pointed at a coordinate.
    pub address: Option<TargetAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAddress {
    pub address: String,
    pub kind: String,
    pub author_pubkey: String,
    pub d_tag: String,
}

impl CoalesceTarget {
    pub fn event(event_id: impl Into<String>) -> Self {
        let event_id = event_id.into();
        Self {
            key: format!("e:{event_id}"),
            event_id: Some(event_id),
            address: None,
        }
    }

    pub fn address(address: TargetAddress) -> Self {
        Self {
            key: format!("a:{}", address.address),
            event_id: None,
            address: Some(address),
        }
    }
}

/// One ingest decision, as recorded in the disposition key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoalesceDecision {
    /// Deliver now. The collapse key must ride on the FCM payload.
    Immediate {
        group_id: String,
        collapse_key: String,
    },
    /// Recorded in the bucket; no push now.
    Buffered { group_id: String },
}

/// Data the ingest script needs from the trigger event.
#[derive(Debug, Clone)]
pub struct Ingest<'a> {
    pub event_id: &'a str,
    pub event_kind: u16,
    pub actor: &'a str,
    pub created_at: u64,
    pub notification_type: NotificationType,
    pub target: &'a CoalesceTarget,
}

/// A claim on one due group. `token` is this worker's ownership proof;
/// `claimed_at` is the Redis server time the claim was granted, which the
/// logical-expiry checks compare against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupClaim {
    pub group_id: String,
    pub token: String,
    pub claimed_at: u64,
}

/// The buffered state of one group, read after its deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSnapshot {
    pub group_id: String,
    pub notification_type: String,
    pub owner: String,
    pub target: String,
    pub due: u64,
    /// Absolute logical lifetime, from the Redis server clock. 0 on a legacy
    /// group that predates the field; such a group is flushed, never dropped.
    pub expires_at: u64,
    pub pending: u64,
    /// Immediate sends already spent in this bucket.
    pub immediate: u64,
    pub first_actor: Option<String>,
    pub first_event: Option<String>,
    pub last_at: Option<u64>,
    pub event_kind: Option<String>,
    pub ref_event_id: Option<String>,
    pub ref_address: Option<String>,
    pub ref_kind: Option<String>,
    pub ref_author: Option<String>,
    pub ref_dtag: Option<String>,
    /// Distinct buffered actors, from the HyperLogLog.
    pub actor_count: u64,
}

/// Derive the FCM collapse identity for a group.
///
/// BLAKE3 of the group id, first 16 bytes, hex: deterministic across replicas
/// and 32 characters, within APNs' 64-byte `apns-collapse-id` limit.
pub fn collapse_key_for_group(group_id: &str) -> String {
    let digest = blake3::hash(group_id.as_bytes());
    hex::encode(&digest.as_bytes()[..16])
}

fn disposition_key(event_id: &str, recipient: &PublicKey) -> String {
    format!("{DISP_PREFIX}{event_id}:{}", recipient.to_hex())
}

/// TTL for the per-recipient token bucket.
///
/// The bucket has refilled to capacity after `capacity * refill_secs`, after
/// which an absent key reads the same as a full one, so the TTL only needs the
/// window-plus-grace floor to keep it alive around the boundary it defers.
fn throttle_key_ttl(settings: &ServiceSettings) -> u64 {
    let disp_ttl = settings
        .coalesce_window_secs
        .saturating_add(settings.coalesce_logical_expiry_grace_secs);
    settings
        .recipient_throttle_capacity
        .saturating_mul(settings.recipient_throttle_refill_secs)
        .saturating_mul(2)
        .max(disp_ttl)
}

/// TTL for the per-recipient emission window.
///
/// A member older than the window stops counting on the next read, so the key
/// only needs the window plus the cleanup grace to stay readable at the
/// boundary.
fn emitted_key_ttl(settings: &ServiceSettings) -> u64 {
    settings
        .recipient_daily_window_secs
        .saturating_add(settings.coalesce_logical_expiry_grace_secs)
}

/// Why a recipient-budget spend did or did not emit a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendOutcome {
    /// The token and the rolling-window slot were spent.
    Spent,
    /// The token bucket is empty; retry after the refill window.
    BucketEmpty { retry_after_secs: u64 },
    /// The rolling emission window is full; retry when the oldest emission
    /// ages out, not on the token refill cadence.
    DailyCapped { retry_after_secs: u64 },
}

/// Spend one token from a recipient's bucket, refilling lazily.
///
/// The immediate path spends a token inside the ingest script. The flush path
/// spends one here, so a summary is an emitted notification sharing the same
/// per-recipient budget: a boundary burst of summaries cannot exceed the
/// bucket, and the remainder defers to a refill.
///
/// Both paths also charge the rolling emission window (`recipient_daily_cap`):
/// a recipient already at the cap cannot spend a token until the oldest
/// emission ages out of the window. `member` identifies this emission in that
/// window — the trigger event id for an immediate push, the group id for a
/// summary — so a retry of the same send cannot count twice.
///
/// A refusal names its bound and the delay that clears it, so a cap deferral
/// waits for the window to slide instead of re-polling the serial drain every
/// refill.
pub async fn consume_recipient_token(
    pool: &RedisPool,
    owner_hex: &str,
    member: &str,
    settings: &ServiceSettings,
) -> Result<SpendOutcome> {
    const TOKEN_SCRIPT: &str = r#"
        local tkey = KEYS[1]
        local ekey = KEYS[2]
        local capacity = tonumber(ARGV[1])
        local refill_secs = tonumber(ARGV[2])
        local throttle_ttl = tonumber(ARGV[3])
        local window = tonumber(ARGV[4])
        local daily_cap = tonumber(ARGV[5])
        local emitted_ttl = tonumber(ARGV[6])
        local member = ARGV[7]

        local now = tonumber(redis.call('TIME')[1])
        local now_ms = now * 1000
        -- The rolling window slides on every read: a recipient at the cap is
        -- refused until the oldest emission ages out.
        redis.call('ZREMRANGEBYSCORE', ekey, '-inf', now - window)
        -- A recovered lease may retry after reserving this logical emission but
        -- before delivery or completion. Reuse that reservation: blocking it on
        -- its own cap entry can defer unsent work until logical expiry, while
        -- spending again leaks another bucket token.
        if redis.call('ZSCORE', ekey, member) ~= false then
          return {1, 0}
        end
        if redis.call('ZCARD', ekey) >= daily_cap then
          local oldest = redis.call('ZRANGE', ekey, 0, 0, 'WITHSCORES')
          local wait = window
          if oldest[2] then
            wait = math.max(1, math.ceil(tonumber(oldest[2]) + window - now))
          end
          return {-1, wait}
        end
        local tokens = tonumber(redis.call('HGET', tkey, 'tokens'))
        local ts = tonumber(redis.call('HGET', tkey, 'ts'))
        if tokens == nil then tokens = capacity end
        if ts == nil then ts = now_ms end
        local elapsed = now_ms - ts
        if elapsed < 0 then elapsed = 0 end
        tokens = math.min(capacity, tokens + elapsed / (refill_secs * 1000))
        if tokens >= 1 then
          tokens = tokens - 1
          redis.call('HSET', tkey, 'tokens', tokens, 'ts', now_ms)
          redis.call('EXPIRE', tkey, throttle_ttl)
          redis.call('ZADD', ekey, now, member)
          redis.call('EXPIRE', ekey, emitted_ttl)
          return {1, 0}
        end
        redis.call('HSET', tkey, 'tokens', tokens, 'ts', now_ms)
        redis.call('EXPIRE', tkey, throttle_ttl)
        return {0, refill_secs}
    "#;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;

    let outcome: (i64, u64) = redis::Script::new(TOKEN_SCRIPT)
        .key(format!("{THROTTLE_PREFIX}{owner_hex}"))
        .key(format!("{EMITTED_PREFIX}{owner_hex}"))
        .arg(settings.recipient_throttle_capacity)
        .arg(settings.recipient_throttle_refill_secs)
        .arg(throttle_key_ttl(settings))
        .arg(settings.recipient_daily_window_secs)
        .arg(settings.recipient_daily_cap)
        .arg(emitted_key_ttl(settings))
        .arg(member)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(match outcome.0 {
        1 => SpendOutcome::Spent,
        0 => SpendOutcome::BucketEmpty {
            retry_after_secs: outcome.1.max(1),
        },
        _ => SpendOutcome::DailyCapped {
            retry_after_secs: outcome.1.max(1),
        },
    })
}

/// Return one token to a recipient's bucket after a flush that emitted nothing.
///
/// The bucket charges only for emitted notifications: a summary that failed
/// retryably on every token never reached a device, so leaving the charge in
/// place would let one stuck group drain each refill and starve the
/// recipient's immediate like/repost pushes for as long as the group retries.
/// The emission also leaves the rolling window, for the same reason.
pub async fn refund_recipient_token(
    pool: &RedisPool,
    owner_hex: &str,
    member: &str,
    settings: &ServiceSettings,
) -> Result<()> {
    const REFUND_SCRIPT: &str = r#"
        local tkey = KEYS[1]
        local ekey = KEYS[2]
        local capacity = tonumber(ARGV[1])
        local refill_secs = tonumber(ARGV[2])
        local throttle_ttl = tonumber(ARGV[3])
        local member = ARGV[4]

        local now_ms = tonumber(redis.call('TIME')[1]) * 1000
        local tokens = tonumber(redis.call('HGET', tkey, 'tokens'))
        local ts = tonumber(redis.call('HGET', tkey, 'ts'))
        if tokens == nil then tokens = capacity end
        if ts == nil then ts = now_ms end
        local elapsed = now_ms - ts
        if elapsed < 0 then elapsed = 0 end
        tokens = math.min(capacity, tokens + elapsed / (refill_secs * 1000) + 1)
        redis.call('HSET', tkey, 'tokens', tokens, 'ts', now_ms)
        redis.call('EXPIRE', tkey, throttle_ttl)
        -- Nothing emitted, so this attempt does not count against the cap.
        redis.call('ZREM', ekey, member)
        return 1
    "#;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;

    let _: i64 = redis::Script::new(REFUND_SCRIPT)
        .key(format!("{THROTTLE_PREFIX}{owner_hex}"))
        .key(format!("{EMITTED_PREFIX}{owner_hex}"))
        .arg(settings.recipient_throttle_capacity)
        .arg(settings.recipient_throttle_refill_secs)
        .arg(throttle_key_ttl(settings))
        .arg(member)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(())
}

/// Decide and record one like/repost ingest atomically.
///
/// Returns the stored decision on a replay. Callers must hold the
/// `(event_id, recipient)` claim before calling: the claim is what keeps two
/// replicas from processing the same event, and the disposition then makes any
/// replay of a released claim reproduce the original decision.
pub async fn ingest(
    pool: &RedisPool,
    settings: &ServiceSettings,
    recipient: &PublicKey,
    ingest: &Ingest<'_>,
) -> Result<CoalesceDecision> {
    const INGEST_SCRIPT: &str = r#"
        local disp_key = KEYS[1]
        local due_key = KEYS[2]

        local window = tonumber(ARGV[1])
        local limit = tonumber(ARGV[2])
        -- Physical TTL: the logical lifetime plus the cleanup grace. The
        -- dropping paths delete and count at the logical lifetime, while this
        -- longer TTL only reclaims genuinely abandoned state.
        local ttl = tonumber(ARGV[3])
        local capacity = tonumber(ARGV[4])
        local refill_secs = tonumber(ARGV[5])
        local throttle_ttl = tonumber(ARGV[6])
        local group_prefix = ARGV[7]
        local hll_prefix = ARGV[8]
        local throttle_prefix = ARGV[9]
        local ntype = ARGV[10]
        local owner = ARGV[11]
        local target = ARGV[12]
        local actor = ARGV[13]
        local event_id = ARGV[14]
        local event_kind = ARGV[15]
        local created_at = tonumber(ARGV[16])
        local ref_event_id = ARGV[17]
        local ref_address = ARGV[18]
        local ref_kind = ARGV[19]
        local ref_author = ARGV[20]
        local ref_dtag = ARGV[21]
        local logical_ttl = tonumber(ARGV[22])
        local disp_ttl = tonumber(ARGV[23])
        local emitted_prefix = ARGV[24]
        local daily_window = tonumber(ARGV[25])
        local daily_cap = tonumber(ARGV[26])
        local emitted_ttl = tonumber(ARGV[27])

        -- Replay: the same decision and therefore the same collapse id.
        local stored = redis.call('GET', disp_key)
        if stored then
          local code, gid = string.match(stored, '^(%a):(.+)$')
          if code == 'i' then return {1, gid} end
          if code == 'b' then return {2, gid} end
        end

        local now = tonumber(redis.call('TIME')[1])
        local bucket = math.floor(now / window)
        local due_at = (bucket + 1) * window
        local gid = ntype .. ':' .. owner .. ':' .. target .. ':' .. bucket
        local gkey = group_prefix .. gid

        local immediate = tonumber(redis.call('HGET', gkey, 'immediate') or '0')
        local send_immediately = false
        local throttled = false

        if immediate < limit then
          local tkey = throttle_prefix .. owner
          local ekey = emitted_prefix .. owner
          local now_ms = now * 1000
          -- The rolling emission window is the outer bound: at the cap the
          -- event is buffered even when the bucket still has tokens, and the
          -- oldest emission aging out is what admits the next push. The window
          -- slides on every read.
          redis.call('ZREMRANGEBYSCORE', ekey, '-inf', now - daily_window)
          local emitted = redis.call('ZCARD', ekey)
          local tokens = tonumber(redis.call('HGET', tkey, 'tokens'))
          local ts = tonumber(redis.call('HGET', tkey, 'ts'))
          if tokens == nil then tokens = capacity end
          if ts == nil then ts = now_ms end
          local elapsed = now_ms - ts
          if elapsed < 0 then elapsed = 0 end
          tokens = math.min(capacity, tokens + elapsed / (refill_secs * 1000))
          if emitted >= daily_cap then
            throttled = true
          elseif tokens >= 1 then
            tokens = tokens - 1
            send_immediately = true
            redis.call('ZADD', ekey, now, event_id)
            redis.call('EXPIRE', ekey, emitted_ttl)
          else
            throttled = true
          end
          redis.call('HSET', tkey, 'tokens', tokens, 'ts', now_ms)
          redis.call('EXPIRE', tkey, throttle_ttl)
        end

        redis.call('HSETNX', gkey, 'due', due_at)
        redis.call('HSETNX', gkey, 'expires_at', now + logical_ttl)
        redis.call('HSETNX', gkey, 'owner', owner)
        redis.call('HSETNX', gkey, 'type', ntype)
        redis.call('HSETNX', gkey, 'target', target)
        redis.call('HSETNX', gkey, 'event_kind', event_kind)
        if ref_event_id ~= '' then
          redis.call('HSETNX', gkey, 'ref_event_id', ref_event_id)
        end
        if ref_address ~= '' then
          redis.call('HSETNX', gkey, 'ref_address', ref_address)
          redis.call('HSETNX', gkey, 'ref_kind', ref_kind)
          redis.call('HSETNX', gkey, 'ref_author', ref_author)
          redis.call('HSETNX', gkey, 'ref_dtag', ref_dtag)
        end

        if send_immediately then
          redis.call('HINCRBY', gkey, 'immediate', 1)
          local pending = tonumber(redis.call('HGET', gkey, 'pending') or '0')
          if pending > 0 then
            redis.call('EXPIRE', gkey, ttl)
          else
            -- An immediate-only group is never claimed; it only needs to
            -- outlive its bucket so a later buffered event in the same bucket
            -- finds the immediate count and routing fields.
            redis.call('EXPIRE', gkey, disp_ttl)
          end
          redis.call('SET', disp_key, 'i:' .. gid, 'EX', disp_ttl)
          return {1, gid}
        end

        redis.call('HINCRBY', gkey, 'pending', 1)
        redis.call('HSETNX', gkey, 'first_actor', actor)
        redis.call('HSETNX', gkey, 'first_event', event_id)
        local last_at = tonumber(redis.call('HGET', gkey, 'last_at') or '0')
        if created_at > last_at then
          redis.call('HSET', gkey, 'last_at', created_at)
        end
        redis.call('PFADD', hll_prefix .. gid, actor)
        redis.call('EXPIRE', gkey, ttl)
        redis.call('EXPIRE', hll_prefix .. gid, ttl)
        redis.call('ZADD', due_key, 'NX', due_at, gid)
        redis.call('SET', disp_key, 'b:' .. gid, 'EX', disp_ttl)
        if throttled then
          return {3, gid}
        end
        return {2, gid}
    "#;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;

    let window = settings.coalesce_window_secs;
    let grace = settings.coalesce_logical_expiry_grace_secs;
    // Two lifetimes: the logical one the dropping paths count while the group
    // is still readable, and the longer physical TTL that only reclaims
    // genuinely abandoned state.
    let logical_ttl = settings.coalesce_group_ttl_secs;
    let physical_ttl = logical_ttl.saturating_add(grace);
    let disp_ttl = window.saturating_add(grace);
    let throttle_ttl = throttle_key_ttl(settings);

    let ref_event_id = ingest.target.event_id.clone().unwrap_or_default();
    let (ref_address, ref_kind, ref_author, ref_dtag) = match &ingest.target.address {
        Some(address) => (
            address.address.clone(),
            address.kind.clone(),
            address.author_pubkey.clone(),
            address.d_tag.clone(),
        ),
        None => (String::new(), String::new(), String::new(), String::new()),
    };

    let decision: (i64, String) = redis::Script::new(INGEST_SCRIPT)
        .key(disposition_key(ingest.event_id, recipient))
        .key(DUE_KEY)
        .arg(window)
        .arg(settings.coalesce_immediate_limit)
        .arg(physical_ttl)
        .arg(settings.recipient_throttle_capacity)
        .arg(settings.recipient_throttle_refill_secs)
        .arg(throttle_ttl)
        .arg(GROUP_PREFIX)
        .arg(HLL_PREFIX)
        .arg(THROTTLE_PREFIX)
        .arg(ingest.notification_type.display_name())
        .arg(recipient.to_hex())
        .arg(&ingest.target.key)
        .arg(ingest.actor)
        .arg(ingest.event_id)
        .arg(ingest.event_kind)
        .arg(ingest.created_at)
        .arg(ref_event_id)
        .arg(ref_address)
        .arg(ref_kind)
        .arg(ref_author)
        .arg(ref_dtag)
        .arg(logical_ttl)
        .arg(disp_ttl)
        .arg(EMITTED_PREFIX)
        .arg(settings.recipient_daily_window_secs)
        .arg(settings.recipient_daily_cap)
        .arg(emitted_key_ttl(settings))
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    let collapse_key = collapse_key_for_group(&decision.1);
    match decision.0 {
        1 => Ok(CoalesceDecision::Immediate {
            group_id: decision.1,
            collapse_key,
        }),
        // 2 = buffered because the immediate budget is spent, 3 = buffered
        // because the recipient's notification budget was spent (empty token
        // bucket or full rolling emission window). Both are buffered; only the
        // metric differs, and the caller reads it from this code.
        3 => {
            crate::metrics::throttled_recipient(ingest.notification_type.display_name());
            Ok(CoalesceDecision::Buffered {
                group_id: decision.1,
            })
        }
        _ => Ok(CoalesceDecision::Buffered {
            group_id: decision.1,
        }),
    }
}

/// Claim the oldest due group, reconciling expired leases first.
///
/// Expired entries are handled inside the same atomic script: an entry with
/// nothing pending is deleted (never re-added), and a pending entry returns to
/// the due queue only when it is absent from both indexes.
pub async fn claim_due_group(pool: &RedisPool, lease_secs: u64) -> Result<Option<GroupClaim>> {
    const CLAIM_SCRIPT: &str = r#"
        local due_key = KEYS[1]
        local leases_key = KEYS[2]
        local group_prefix = ARGV[1]
        local hll_prefix = ARGV[2]
        local lease_secs = tonumber(ARGV[3])
        local max_recovery = tonumber(ARGV[4])
        local token = ARGV[5]
        local max_dangling = tonumber(ARGV[6])

        local now = tonumber(redis.call('TIME')[1])
        local expired_drops = 0
        local dangling_drops = 0

        -- Reconcile expired leases before taking new work.
        local expired = redis.call('ZRANGEBYSCORE', leases_key, '-inf', now, 'LIMIT', 0, max_recovery)
        for _, gid in ipairs(expired) do
          local gkey = group_prefix .. gid
          local pending = tonumber(redis.call('HGET', gkey, 'pending') or '0')
          redis.call('ZREM', leases_key, gid)
          if pending > 0 then
            local expires_at = tonumber(redis.call('HGET', gkey, 'expires_at') or '0')
            if expires_at > 0 and now >= expires_at then
              -- Reached its logical lifetime while queued: drop it here, while
              -- the hash still names the work, rather than letting the physical
              -- TTL erase the evidence silently. Counted by the caller.
              redis.call('DEL', gkey)
              redis.call('DEL', hll_prefix .. gid)
              expired_drops = expired_drops + 1
            elseif redis.call('ZSCORE', due_key, gid) == false and redis.call('ZSCORE', leases_key, gid) == false then
              -- Only expired entries are iterated, so no live flush owns this
              -- group; `due` is the index that can still hold a competing live
              -- entry, and re-adding must be skipped when it does. The `leases`
              -- predicate always reads false at this point (the expired member
              -- was removed above) and is kept as a defensive assertion only.
              local due_at = tonumber(redis.call('HGET', gkey, 'due') or '0')
              if due_at <= 0 then due_at = now end
              redis.call('ZADD', due_key, due_at, gid)
            end
          else
            -- Nothing pending (every event was immediate, or the buffered set
            -- was already flushed): not work, just stale state.
            redis.call('DEL', gkey)
            redis.call('DEL', hll_prefix .. gid)
          end
        end

        local candidates = redis.call('ZRANGEBYSCORE', due_key, '-inf', now, 'LIMIT', 0, max_dangling)
        for _, gid in ipairs(candidates) do
          local gkey = group_prefix .. gid
          if redis.call('EXISTS', gkey) == 0 then
            -- Defensive: physical TTL, eviction, or a legacy build removed the
            -- group under a due member. Counted by the caller.
            redis.call('ZREM', due_key, gid)
            dangling_drops = dangling_drops + 1
          else
            redis.call('ZREM', due_key, gid)
            redis.call('ZADD', leases_key, now + lease_secs, gid)
            redis.call('HSET', gkey, 'lease', token)
            return {gid, expired_drops, dangling_drops, now}
          end
        end
        return {'', expired_drops, dangling_drops, now}
    "#;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;
    let token = Uuid::new_v4().to_string();

    let (claimed_id, expired_drops, dangling_drops, now): (String, u64, u64, u64) =
        redis::Script::new(CLAIM_SCRIPT)
            .key(DUE_KEY)
            .key(LEASES_KEY)
            .arg(GROUP_PREFIX)
            .arg(HLL_PREFIX)
            .arg(lease_secs)
            .arg(MAX_RECOVERY_PER_CLAIM)
            .arg(&token)
            .arg(MAX_DANGLING_SKIPS)
            .invoke_async(&mut *conn)
            .await
            .map_err(ServiceError::Redis)?;

    if expired_drops > 0 {
        crate::metrics::coalesce_skipped("expired", expired_drops);
        warn!(
            count = expired_drops,
            "Dropped coalescing groups that reached their logical expiry before a flush"
        );
    }
    if dangling_drops > 0 {
        crate::metrics::coalesce_skipped("dangling_due", dangling_drops);
        warn!(
            count = dangling_drops,
            "Removed dangling coalescing due members whose group hash was already gone"
        );
    }

    if claimed_id.is_empty() {
        return Ok(None);
    }

    Ok(Some(GroupClaim {
        group_id: claimed_id,
        token,
        claimed_at: now,
    }))
}

/// What one completion attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    /// Finished or dropped; the group and its actor count are deleted.
    Completed,
    /// Returned to the due queue for a later attempt.
    Requeued,
    /// Crossing its logical expiry; deleted and counted instead of requeued.
    Expired,
    /// Another worker's lease owns the group now; nothing was mutated.
    NotOwner,
}

/// Complete a claimed group under its ownership token.
///
/// `requeue_after_secs` is `None` for a finished group (deleted) and
/// `Some(delay)` to return it to the due queue for a later retry. A requeue
/// that would cross the group's `expires_at` deletes and counts it instead,
/// while the hash still names the recipient, type, and target.
pub async fn complete_group(
    pool: &RedisPool,
    claim: &GroupClaim,
    requeue_after_secs: Option<u64>,
) -> Result<CompleteOutcome> {
    const COMPLETE_SCRIPT: &str = r#"
        local leases_key = KEYS[1]
        local due_key = KEYS[2]
        local group_prefix = ARGV[1]
        local hll_prefix = ARGV[2]
        local gid = ARGV[3]
        local token = ARGV[4]
        local requeue_after = ARGV[5]

        local gkey = group_prefix .. gid
        if redis.call('HGET', gkey, 'lease') ~= token then
          -- Another worker's lease owns this group now; never delete its work.
          return 0
        end
        redis.call('ZREM', leases_key, gid)
        local now = tonumber(redis.call('TIME')[1])
        if requeue_after ~= '' then
          local expires_at = tonumber(redis.call('HGET', gkey, 'expires_at') or '0')
          local next_score = now + tonumber(requeue_after)
          redis.call('HDEL', gkey, 'lease')
          if expires_at > 0 and next_score >= expires_at then
            redis.call('DEL', gkey)
            redis.call('DEL', hll_prefix .. gid)
            return 3
          end
          redis.call('ZADD', due_key, next_score, gid)
          return 2
        end
        redis.call('DEL', gkey)
        redis.call('DEL', hll_prefix .. gid)
        return 1
    "#;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;
    let requeue = requeue_after_secs
        .map(|secs| secs.to_string())
        .unwrap_or_default();
    let outcome: i64 = redis::Script::new(COMPLETE_SCRIPT)
        .key(LEASES_KEY)
        .key(DUE_KEY)
        .arg(GROUP_PREFIX)
        .arg(HLL_PREFIX)
        .arg(&claim.group_id)
        .arg(&claim.token)
        .arg(requeue)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(match outcome {
        1 => CompleteOutcome::Completed,
        2 => CompleteOutcome::Requeued,
        3 => CompleteOutcome::Expired,
        _ => CompleteOutcome::NotOwner,
    })
}

/// Age in seconds of the oldest bucket group that is due now.
///
/// Sampled by the worker on every pass, not only when a group is claimed, so
/// the gauge keeps telling the truth while the worker is alive: it rises when
/// the worker falls behind and returns to zero once the queue is drained. A
/// dead worker is covered by `coalesce_flush` in `/health`, since a gauge
/// cannot update itself after the process stops.
///
/// The age is computed from the Redis server clock, the same clock that wrote
/// the due scores, so pod clock skew cannot shift it.
pub async fn oldest_due_age_seconds(pool: &RedisPool) -> Result<u64> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;
    let now: u64 = redis::cmd("TIME")
        .query_async::<(u64, u64)>(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?
        .0;
    let head: Vec<(String, u64)> = redis::cmd("ZRANGEBYSCORE")
        .arg(DUE_KEY)
        .arg("-inf")
        .arg(now)
        .arg("LIMIT")
        .arg(0)
        .arg(1)
        .arg("WITHSCORES")
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(head
        .first()
        .map(|(_, score)| now.saturating_sub(*score))
        .unwrap_or(0))
}

/// Read one group's buffered state plus its distinct-actor count.
///
/// The two reads are not atomic, but a group cannot change after its bucket
/// deadline: ingest computes the next bucket and writes a different key. The
/// claim script only ever claims groups at or after their deadline.
pub async fn load_group(pool: &RedisPool, group_id: &str) -> Result<Option<GroupSnapshot>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {e}")))?;

    let fields: HashMap<String, String> = redis::cmd("HGETALL")
        .arg(format!("{GROUP_PREFIX}{group_id}"))
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    if fields.is_empty() {
        return Ok(None);
    }

    // PFCOUNT answers 0 for a missing key, which is the right reading for a
    // group whose HLL expired: there is nothing to summarize.
    let actor_count: u64 = redis::cmd("PFCOUNT")
        .arg(format!("{HLL_PREFIX}{group_id}"))
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    let get = |name: &str| fields.get(name).cloned();
    let number = |name: &str| {
        get(name)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };

    Ok(Some(GroupSnapshot {
        group_id: group_id.to_string(),
        notification_type: get("type").unwrap_or_default(),
        owner: get("owner").unwrap_or_default(),
        target: get("target").unwrap_or_default(),
        due: number("due"),
        expires_at: number("expires_at"),
        pending: number("pending"),
        immediate: number("immediate"),
        first_actor: get("first_actor"),
        first_event: get("first_event"),
        last_at: get("last_at").and_then(|value| value.parse::<u64>().ok()),
        event_kind: get("event_kind"),
        ref_event_id: get("ref_event_id"),
        ref_address: get("ref_address"),
        ref_kind: get("ref_kind"),
        ref_author: get("ref_author"),
        ref_dtag: get("ref_dtag"),
        actor_count,
    }))
}

/// `N other(s)` for a summary of `distinct_actors`, which is at least two.
fn others_phrase(distinct_actors: u64) -> String {
    if distinct_actors == 2 {
        "1 other".to_string()
    } else {
        format!("{} others", distinct_actors - 1)
    }
}

/// Summary copy for one flush.
///
/// The name is the first buffered actor; the count is distinct actors. A single
/// buffered actor gets the same sentence the immediate push would have used.
pub fn summary_copy(
    notification_type: &str,
    sample_name: &str,
    distinct_actors: u64,
) -> (String, String) {
    let name = if sample_name.is_empty() {
        "Someone"
    } else {
        sample_name
    };
    match (notification_type, distinct_actors) {
        ("repost", 0..=1) => (
            "New repost".to_string(),
            format!("{name} reposted your post"),
        ),
        ("repost", count) => (
            "New reposts".to_string(),
            format!("{name} and {} reposted your post", others_phrase(count)),
        ),
        (_, 0..=1) => ("New like".to_string(), format!("{name} liked your post")),
        (_, count) => (
            "New likes".to_string(),
            format!("{name} and {} liked your post", others_phrase(count)),
        ),
    }
}

/// Build the data-only summary payload for a flushed group.
pub fn build_summary_payload(
    snapshot: &GroupSnapshot,
    recipient: &PublicKey,
    sample_name: &str,
    collapse_key: &str,
) -> FcmPayload {
    let mut data = HashMap::new();

    data.insert("type".to_string(), snapshot.notification_type.clone());
    data.insert(
        "eventId".to_string(),
        snapshot
            .first_event
            .clone()
            .unwrap_or_else(|| snapshot.target.clone()),
    );

    let (title, body) = summary_copy(
        &snapshot.notification_type,
        sample_name,
        snapshot.actor_count,
    );
    data.insert("title".to_string(), title);
    data.insert("body".to_string(), body);
    if let Some(actor) = &snapshot.first_actor {
        data.insert("senderPubkey".to_string(), actor.clone());
    }
    data.insert("senderName".to_string(), sample_name.to_string());
    data.insert("receiverPubkey".to_string(), recipient.to_hex());
    data.insert(
        "receiverNpub".to_string(),
        recipient.to_bech32().unwrap_or_default(),
    );
    data.insert(
        "eventKind".to_string(),
        snapshot.event_kind.clone().unwrap_or_default(),
    );
    data.insert(
        "timestamp".to_string(),
        snapshot.last_at.unwrap_or(snapshot.due).to_string(),
    );

    // Reproduce the same routing fields the immediate payload derives from the
    // trigger event, so a tap on the summary opens the same target.
    if let Some(event_id) = &snapshot.ref_event_id {
        data.insert("referencedEventId".to_string(), event_id.clone());
    }
    if let Some(address) = &snapshot.ref_address {
        data.insert("referencedAddress".to_string(), address.clone());
        if let Some(kind) = &snapshot.ref_kind {
            data.insert("referencedKind".to_string(), kind.clone());
        }
        if let Some(author) = &snapshot.ref_author {
            data.insert("referencedAuthorPubkey".to_string(), author.clone());
        }
        if let Some(d_tag) = &snapshot.ref_dtag {
            data.insert("referencedDTag".to_string(), d_tag.clone());
        }
    }

    FcmPayload {
        notification: None,
        data: Some(data),
        android: None,
        webpush: None,
        apns: None,
        collapse_key: Some(collapse_key.to_string()),
    }
}

fn short_npub(pubkey_hex: &str) -> String {
    match PublicKey::from_hex(pubkey_hex) {
        Ok(pubkey) => pubkey
            .to_bech32()
            .map(|npub| {
                if npub.len() > 12 {
                    format!("{}...", &npub[..12])
                } else {
                    npub
                }
            })
            .unwrap_or_else(|_| "unknown".to_string()),
        Err(_) => "unknown".to_string(),
    }
}

/// What one flush attempt did, for logging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    Sent {
        delivered: usize,
        failed: usize,
        actor_count: u64,
    },
    /// All-token retryable failure; back in the due queue.
    RetryQueued,
    /// Recipient bucket empty; back in the due queue at the refill time.
    Throttled,
    /// Crossed its logical lifetime; deleted and counted instead of sent.
    Expired,
    Suppressed {
        reason: &'static str,
    },
    Empty {
        reason: &'static str,
    },
}

/// Delete a group that crossed its logical lifetime, naming it in the log and
/// counting the loss so it is attributable rather than a silent expiry.
async fn drop_expired_group(
    state: &AppState,
    claim: &GroupClaim,
    snapshot: &GroupSnapshot,
) -> Result<()> {
    match complete_group(&state.redis_pool, claim, None).await? {
        CompleteOutcome::Completed => {
            crate::metrics::coalesce_skipped("expired", 1);
            warn!(
                group_id = %claim.group_id,
                owner = %snapshot.owner,
                ntype = %snapshot.notification_type,
                target = %snapshot.target,
                pending = snapshot.pending,
                "Dropped a coalesced group at its logical expiry without sending a summary"
            );
            Ok(())
        }
        outcome => Err(ServiceError::Internal(format!(
            "expired coalescing group completion was not owned by this worker: {outcome:?}"
        ))),
    }
}

/// Return a claimed group to the due queue, or drop it when the retry would
/// cross its logical lifetime. A drop is logged with the work it names and
/// counted; a requeue reports `requeue_outcome`.
async fn requeue_or_expire(
    state: &AppState,
    claim: &GroupClaim,
    snapshot: &GroupSnapshot,
    requeue_outcome: FlushOutcome,
    delay_secs: u64,
) -> Result<FlushOutcome> {
    match complete_group(&state.redis_pool, claim, Some(delay_secs)).await? {
        CompleteOutcome::Requeued => Ok(requeue_outcome),
        CompleteOutcome::Expired => {
            // A group that expires while its recipient's budget is spent is the
            // expected steady state at the cap, not a backlog incident; give it
            // its own reason so the `expired` counter still isolates backlog
            // losses.
            let reason = if requeue_outcome == FlushOutcome::Throttled {
                "expired_throttled"
            } else {
                "expired"
            };
            crate::metrics::coalesce_skipped(reason, 1);
            warn!(
                group_id = %claim.group_id,
                owner = %snapshot.owner,
                ntype = %snapshot.notification_type,
                target = %snapshot.target,
                pending = snapshot.pending,
                reason,
                "Dropped a coalesced group at its logical expiry instead of requeueing it"
            );
            Ok(FlushOutcome::Expired)
        }
        outcome => Err(ServiceError::Internal(format!(
            "coalescing requeue was not owned by this worker: {outcome:?}"
        ))),
    }
}

/// Process one claimed group. Always attempts to complete the claim; an error
/// means the group could not even be completed and the caller should requeue.
pub(crate) async fn flush_group(
    state: &AppState,
    claim: &GroupClaim,
    token: &CancellationToken,
) -> Result<FlushOutcome> {
    let Some(snapshot) = load_group(&state.redis_pool, &claim.group_id).await? else {
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("missing_group", 1);
        info!(
            group_id = %claim.group_id,
            "Coalesced group hash was already gone when its flush ran"
        );
        return Ok(FlushOutcome::Empty {
            reason: "missing_group",
        });
    };

    // The logical lifetime is where a backlog or a permanent throttle is
    // declared lost. Do it here, where the hash still names the work, rather
    // than letting the physical TTL erase the evidence.
    if snapshot.expires_at > 0 && snapshot.expires_at <= claim.claimed_at {
        drop_expired_group(state, claim, &snapshot).await?;
        return Ok(FlushOutcome::Expired);
    }

    let Some(notification_type) = NotificationType::from_display_name(&snapshot.notification_type)
    else {
        warn!(
            group_id = %claim.group_id,
            ntype = %snapshot.notification_type,
            "Discarding coalesced group with an unknown notification type"
        );
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("unknown_type", 1);
        return Ok(FlushOutcome::Empty {
            reason: "unknown_type",
        });
    };

    if snapshot.pending == 0 {
        // Routine reconciliation: every event was sent immediately, or the
        // buffered set was already flushed before this claim landed.
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("no_pending", 1);
        return Ok(FlushOutcome::Empty {
            reason: "no_pending",
        });
    }
    if snapshot.actor_count == 0 {
        // The hyperloglog is written and expired with the group, so this means
        // it was evicted or lost independently. Dropping the summary loses no
        // notification: the individual events remain in the recipient's inbox.
        warn!(
            group_id = %claim.group_id,
            pending = snapshot.pending,
            "Discarding coalesced group whose actor counter is missing"
        );
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("missing_actor_count", 1);
        return Ok(FlushOutcome::Empty {
            reason: "missing_actor_count",
        });
    }

    let owner = match PublicKey::from_hex(&snapshot.owner) {
        Ok(owner) => owner,
        Err(e) => {
            warn!(group_id = %claim.group_id, error = %e, "Discarding coalesced group with an unparseable owner");
            complete_group(&state.redis_pool, claim, None).await?;
            crate::metrics::coalesce_skipped("unparseable_owner", 1);
            return Ok(FlushOutcome::Empty {
                reason: "unparseable_owner",
            });
        }
    };

    // Revalidate every gate the immediate path applies, because all of them can
    // have changed since the event was buffered. Each drop is counted so the
    // suppression is attributable.
    let allowed = &state.settings.service.allowed_pubkeys;
    if !allowed.is_empty() && !allowed.contains(&snapshot.owner) {
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("not_allowlisted", 1);
        info!(
            group_id = %claim.group_id,
            owner = %snapshot.owner,
            "Suppressed a coalesced summary; the recipient is not allowlisted"
        );
        return Ok(FlushOutcome::Suppressed {
            reason: "not_allowlisted",
        });
    }

    let tokens = redis_store::get_tokens_for_pubkey(&state.redis_pool, &owner).await?;
    if tokens.is_empty() {
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("no_tokens", 1);
        info!(
            group_id = %claim.group_id,
            owner = %snapshot.owner,
            "Suppressed a coalesced summary; the recipient has no registered tokens"
        );
        return Ok(FlushOutcome::Suppressed {
            reason: "no_tokens",
        });
    }

    let prefs = preferences::get_user_preferences(
        &state.redis_pool,
        &snapshot.owner,
        &state.settings.notification.default_preferences,
    )
    .await?;
    if !notification_type.is_enabled(&prefs) {
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("preference_disabled", 1);
        info!(
            group_id = %claim.group_id,
            owner = %snapshot.owner,
            ntype = %snapshot.notification_type,
            "Suppressed a coalesced summary; the recipient disabled this type"
        );
        return Ok(FlushOutcome::Suppressed {
            reason: "preference_disabled",
        });
    }

    if token.is_cancelled() {
        return Err(ServiceError::Cancelled);
    }

    // A summary is an emitted notification, so it spends the same
    // per-recipient budget the immediate path spends: a token from the bucket
    // and a slot in the rolling emission window. That is what bounds a
    // boundary burst of summaries, and it is what holds a recipient at
    // `recipient_daily_cap` even when the bucket has refilled. A deferral is
    // not a failure and does not touch the retry path. Each bound reports the
    // delay that clears it: a refill for an empty bucket, the oldest emission
    // aging out for a full window, so a capped group does not re-poll the
    // serial drain every minute for as long as its lifetime.
    let recipient = snapshot.owner.clone();
    let deferral_secs = match consume_recipient_token(
        &state.redis_pool,
        &recipient,
        &claim.group_id,
        &state.settings.service,
    )
    .await?
    {
        SpendOutcome::Spent => None,
        SpendOutcome::BucketEmpty { retry_after_secs } => {
            crate::metrics::coalesce_deferred("recipient_throttled", 1);
            info!(
                group_id = %claim.group_id,
                owner = %snapshot.owner,
                ntype = %snapshot.notification_type,
                retry_after_secs,
                "Deferring a coalesced summary; the recipient token bucket is empty"
            );
            Some(retry_after_secs)
        }
        SpendOutcome::DailyCapped { retry_after_secs } => {
            crate::metrics::coalesce_deferred("recipient_daily_capped", 1);
            info!(
                group_id = %claim.group_id,
                owner = %snapshot.owner,
                ntype = %snapshot.notification_type,
                retry_after_secs,
                daily_cap = state.settings.service.recipient_daily_cap,
                "Deferring a coalesced summary; the recipient is at the rolling emission cap"
            );
            Some(retry_after_secs)
        }
    };
    if let Some(delay_secs) = deferral_secs {
        return requeue_or_expire(state, claim, &snapshot, FlushOutcome::Throttled, delay_secs)
            .await;
    }

    let sample_name = match (&state.mention_parser_service, &snapshot.first_actor) {
        (Some(parser), Some(actor)) => match parser.get_display_name(actor).await {
            Ok(Some(name)) => name,
            Ok(None) => short_npub(actor),
            Err(e) => {
                warn!(group_id = %claim.group_id, error = %e, "Failed to resolve summary actor name");
                short_npub(actor)
            }
        },
        (_, Some(actor)) => short_npub(actor),
        (_, None) => "Someone".to_string(),
    };

    let collapse_key = collapse_key_for_group(&claim.group_id);
    let payload = build_summary_payload(&snapshot, &owner, &sample_name, &collapse_key);

    info!(
        group_id = %claim.group_id,
        owner = %snapshot.owner,
        ntype = %snapshot.notification_type,
        actor_count = snapshot.actor_count,
        token_count = tokens.len(),
        "Flushing coalesced notification group"
    );

    let results = state.fcm_client.send_batch(&tokens, payload).await;

    let mut delivered = Vec::new();
    let mut tokens_to_remove = Vec::new();
    let mut failed = 0usize;
    let mut retryable_failure = None;
    for (fcm_token, result) in results {
        match result {
            Ok(()) => delivered.push(fcm_token),
            Err(FcmError::TokenNotRegistered) => {
                failed += 1;
                tokens_to_remove.push(fcm_token);
            }
            Err(error) => {
                failed += 1;
                if let FcmError::RetryableInternal(delay) = &error {
                    retryable_failure = Some(*delay);
                }
                error!(
                    group_id = %claim.group_id,
                    token_prefix = %crate::fcm_sender::token_prefix(&fcm_token),
                    error = %error,
                    "FCM summary send failed for token"
                );
            }
        }
    }

    // Bookkeeping about a push that may already have shipped: log and continue
    // rather than reporting the delivery as failed.
    if !delivered.is_empty() {
        if let Err(e) = redis_store::refresh_token_activity(&state.redis_pool, &delivered).await {
            error!(group_id = %claim.group_id, error = %e, "Failed to refresh token activity after a delivered summary");
        }
    }
    for fcm_token in tokens_to_remove {
        match redis_store::remove_token(&state.redis_pool, &owner, &fcm_token).await {
            Ok(removed) => {
                if removed {
                    crate::metrics::tokens_pruned("invalid", 1);
                }
            }
            Err(e) => {
                error!(group_id = %claim.group_id, error = %e, "Failed to remove invalid token after a summary");
            }
        }
    }

    if delivered.is_empty() {
        // Nothing reached a device, so the emitted-notification charge is
        // returned — the token and the rolling-window slot — whether the
        // failure was retryable or terminal. Charging for a notification nobody
        // received would hold budget the recipient can spend on a push that
        // does arrive.
        if let Err(e) = refund_recipient_token(
            &state.redis_pool,
            &recipient,
            &claim.group_id,
            &state.settings.service,
        )
        .await
        {
            error!(group_id = %claim.group_id, error = %e, "Failed to refund the recipient throttle token after a summary failure that delivered nothing");
        }
        if let Some(delay) = retryable_failure {
            // FCM's Retry-After wins when it asks for more than the configured
            // minimum. A requeue that would cross the logical lifetime drops
            // the group instead.
            let retry_secs = delay
                .as_secs()
                .max(state.settings.service.coalesce_retry_secs);
            return requeue_or_expire(
                state,
                claim,
                &snapshot,
                FlushOutcome::RetryQueued,
                retry_secs,
            )
            .await;
        }
        // Every token failed non-retryably: the group is terminal and would
        // otherwise be deleted with no outcome recorded, so count and log the
        // loss the way the other terminal paths do.
        complete_group(&state.redis_pool, claim, None).await?;
        crate::metrics::coalesce_skipped("send_failed", 1);
        warn!(
            group_id = %claim.group_id,
            owner = %snapshot.owner,
            ntype = %snapshot.notification_type,
            target = %snapshot.target,
            failed,
            "Dropped a coalesced group whose every send failed non-retryably"
        );
        return Ok(FlushOutcome::Sent {
            delivered: 0,
            failed,
            actor_count: snapshot.actor_count,
        });
    }

    complete_group(&state.redis_pool, claim, None).await?;
    crate::metrics::coalesced_send(notification_type.display_name());
    Ok(FlushOutcome::Sent {
        delivered: delivered.len(),
        failed,
        actor_count: snapshot.actor_count,
    })
}

/// Runs the leased coalescing outbox. Both replicas run this worker.
pub async fn run_coalesce_flush(state: Arc<AppState>, token: CancellationToken) -> Result<()> {
    info!("Starting coalescing flush worker...");
    let poll_interval = Duration::from_millis(state.settings.service.coalesce_poll_millis);

    loop {
        if token.is_cancelled() {
            break;
        }

        // Sample the true head of the due queue on every pass so the gauge
        // reports a growing backlog while this worker is alive but behind.
        match oldest_due_age_seconds(&state.redis_pool).await {
            Ok(age) => crate::metrics::coalesce_oldest_due_age(age as f64),
            Err(e) => {
                warn!(error = %e, "Failed to sample the oldest due coalescing group");
            }
        }

        let claim = tokio::select! {
            biased;
            _ = token.cancelled() => break,
            result = claim_due_group(
                &state.redis_pool,
                state.settings.service.coalesce_lease_secs,
            ) => result,
        };

        match claim {
            Ok(Some(claim)) => {
                // Hard bound on one flush: a stalled profile lookup or FCM
                // batch must not pin the drain behind this group. On timeout
                // the lease is left in place, so expiry recovers the work.
                let flush_timeout =
                    Duration::from_secs(state.settings.service.coalesce_flush_timeout_secs);
                let flushed =
                    tokio::time::timeout(flush_timeout, flush_group(&state, &claim, &token)).await;

                match flushed {
                    Ok(Ok(outcome)) => {
                        tracing::debug!(
                            group_id = %claim.group_id,
                            outcome = ?outcome,
                            "Coalesced group flush finished"
                        );
                    }
                    Ok(Err(ServiceError::Cancelled)) => {
                        if let Err(release_error) =
                            complete_group(&state.redis_pool, &claim, Some(0)).await
                        {
                            error!(error = %release_error, "Failed to release a coalesced group during shutdown; lease expiry will recover it");
                        }
                        break;
                    }
                    Ok(Err(e)) => {
                        crate::metrics::coalesce_flush_failure("error", 1);
                        error!(error = %e, "Coalesced group flush failed; requeuing");
                        match complete_group(
                            &state.redis_pool,
                            &claim,
                            Some(state.settings.service.coalesce_retry_secs),
                        )
                        .await
                        {
                            Ok(CompleteOutcome::Expired) => {
                                crate::metrics::coalesce_skipped("expired", 1);
                                warn!(
                                    group_id = %claim.group_id,
                                    "Dropped a coalesced group at its logical expiry after a flush error"
                                );
                            }
                            Ok(_) => {}
                            Err(requeue_error) => {
                                error!(error = %requeue_error, "Failed to requeue a coalesced group after a flush error; lease expiry will recover it");
                            }
                        }
                    }
                    Err(_) => {
                        crate::metrics::coalesce_flush_failure("timeout", 1);
                        error!(
                            group_id = %claim.group_id,
                            timeout_secs = state.settings.service.coalesce_flush_timeout_secs,
                            "Coalesced group flush exceeded its timeout; lease expiry will recover it"
                        );
                    }
                }
            }
            Ok(None) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(poll_interval) => {}
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to claim a coalesced group");
                tokio::select! {
                    biased;
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(poll_interval) => {}
                }
            }
        }
    }

    info!("Coalescing flush worker shut down.");
    Ok(())
}

/// Serializes tests that share the global coalescing indexes.
#[cfg(test)]
pub(crate) fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fcm_sender::{FcmClient, FcmSend};
    use crate::models::FcmPayload;
    use async_trait::async_trait;
    use nostr_sdk::{Keys, Timestamp};
    use std::sync::Arc;

    async fn test_pool() -> Option<RedisPool> {
        let base_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
        let mut redis_url = url::Url::parse(&base_url).ok()?;
        // Keep this suite's global indexes out of every other test's keyspace.
        redis_url.set_path("/15");
        let pool = redis_store::create_pool(redis_url.as_str(), 3).await.ok()?;
        let mut conn = pool.get().await.ok()?;
        let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
        drop(conn);
        pong.ok().map(|_| pool)
    }

    fn test_settings() -> ServiceSettings {
        let mut service = crate::config::Settings::new()
            .expect("runtime settings load")
            .service;
        service.allowed_pubkeys.clear();
        service
    }

    fn random_target() -> CoalesceTarget {
        CoalesceTarget::event(Uuid::new_v4().to_string())
    }

    fn random_actor() -> String {
        Keys::generate().public_key().to_hex()
    }

    async fn reset_indexes(pool: &RedisPool) {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(DUE_KEY)
            .arg(LEASES_KEY)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
    }

    async fn ingest_one(
        pool: &RedisPool,
        settings: &ServiceSettings,
        owner: &PublicKey,
        target: &CoalesceTarget,
        event_id: &str,
        actor: &str,
    ) -> CoalesceDecision {
        ingest(
            pool,
            settings,
            owner,
            &Ingest {
                event_id,
                event_kind: 7,
                actor,
                created_at: Timestamp::now().as_secs(),
                notification_type: NotificationType::Like,
                target,
            },
        )
        .await
        .expect("ingest should succeed")
    }

    /// Make a group claimable now, the way a real bucket deadline would.
    async fn make_claimable(pool: &RedisPool, gid: &str) {
        let now = Timestamp::now().as_secs();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("HSET")
            .arg(format!("{GROUP_PREFIX}{gid}"))
            .arg("due")
            .arg(now)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
        redis::cmd("ZADD")
            .arg(DUE_KEY)
            .arg(now)
            .arg(gid)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
    }

    async fn cleanup_group(pool: &RedisPool, gid: &str) {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("{GROUP_PREFIX}{gid}"))
            .arg(format!("{HLL_PREFIX}{gid}"))
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
        redis::cmd("ZREM")
            .arg(DUE_KEY)
            .arg(LEASES_KEY)
            .arg(gid)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
    }

    async fn stored_disposition(
        pool: &RedisPool,
        event_id: &str,
        owner: &PublicKey,
    ) -> Option<String> {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("GET")
            .arg(disposition_key(event_id, owner))
            .query_async(&mut *conn)
            .await
            .unwrap()
    }

    fn test_state(
        settings: crate::config::Settings,
        pool: RedisPool,
        fcm_client: FcmClient,
    ) -> AppState {
        AppState {
            settings,
            redis_pool: pool,
            fcm_client: Arc::new(fcm_client),
            service_keys: None,
            crypto_service: None,
            nostr_client: Arc::new(nostr_sdk::Client::default()),
            profile_client: Arc::new(nostr_sdk::Client::default()),
            mention_parser_service: None,
        }
    }

    #[test]
    fn collapse_key_is_stable_and_within_the_apns_limit() {
        let group = "like:owner:target:42";
        let key = collapse_key_for_group(group);
        assert_eq!(key, collapse_key_for_group(group));
        assert_eq!(key.len(), 32, "16 bytes of hex");
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(key, collapse_key_for_group("like:owner:target:43"));
        assert!(key.len() <= 64, "APNs caps apns-collapse-id at 64 bytes");
    }

    #[test]
    fn summary_copy_uses_the_distinct_actor_count() {
        assert_eq!(
            summary_copy("like", "alice", 1),
            ("New like".to_string(), "alice liked your post".to_string())
        );
        assert_eq!(
            summary_copy("like", "alice", 2),
            (
                "New likes".to_string(),
                "alice and 1 other liked your post".to_string()
            )
        );
        assert_eq!(
            summary_copy("like", "alice", 13),
            (
                "New likes".to_string(),
                "alice and 12 others liked your post".to_string()
            )
        );
        assert_eq!(
            summary_copy("repost", "bob", 2),
            (
                "New reposts".to_string(),
                "bob and 1 other reposted your post".to_string()
            )
        );
        assert_eq!(
            summary_copy("repost", "bob", 1),
            (
                "New repost".to_string(),
                "bob reposted your post".to_string()
            )
        );
        assert_eq!(
            summary_copy("like", "", 4),
            (
                "New likes".to_string(),
                "Someone and 3 others liked your post".to_string()
            )
        );
    }

    #[test]
    fn summary_payload_carries_routing_and_collapse_fields() {
        let recipient = Keys::generate().public_key();
        let snapshot = GroupSnapshot {
            group_id: "like:owner:e:target:1".to_string(),
            notification_type: "like".to_string(),
            owner: recipient.to_hex(),
            target: "e:target".to_string(),
            due: 1_700_000_000,
            expires_at: 1_700_086_400,
            pending: 4,
            immediate: 3,
            first_actor: Some("a".repeat(64)),
            first_event: Some("b".repeat(64)),
            last_at: Some(1_699_999_999),
            event_kind: Some("7".to_string()),
            ref_event_id: Some("c".repeat(64)),
            ref_address: None,
            ref_kind: None,
            ref_author: None,
            ref_dtag: None,
            actor_count: 4,
        };

        let payload = build_summary_payload(&snapshot, &recipient, "alice", "collapsequay");
        assert_eq!(payload.collapse_key.as_deref(), Some("collapsequay"));
        assert!(payload.notification.is_none());
        let data = payload.data.expect("data-only payload");
        assert_eq!(data.get("type"), Some(&"like".to_string()));
        assert_eq!(
            data.get("body"),
            Some(&"alice and 3 others liked your post".to_string())
        );
        assert_eq!(data.get("eventId"), Some(&"b".repeat(64)));
        assert_eq!(data.get("referencedEventId"), Some(&"c".repeat(64)));
        assert_eq!(data.get("eventKind"), Some(&"7".to_string()));
        assert_eq!(data.get("receiverPubkey"), Some(&recipient.to_hex()));
    }

    #[tokio::test]
    async fn first_events_send_immediately_and_the_rest_buffer() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 3;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let mut events: Vec<(String, String, CoalesceDecision)> = Vec::new();
        for _ in 0..4 {
            let event_id = Uuid::new_v4().to_string();
            let actor = random_actor();
            let decision = ingest_one(&pool, &settings, &owner, &target, &event_id, &actor).await;
            events.push((event_id, actor, decision));
        }

        for (_, _, decision) in &events[..3] {
            assert!(
                matches!(decision, CoalesceDecision::Immediate { .. }),
                "the first three interactions must send immediately"
            );
        }
        let CoalesceDecision::Buffered { group_id } = &events[3].2 else {
            panic!("the fourth interaction must buffer");
        };

        // A replay reproduces the same decision, group, and collapse id without
        // double-counting.
        let (event_id, actor, decision) = &events[3];
        let replay = ingest_one(&pool, &settings, &owner, &target, event_id, actor).await;
        assert_eq!(replay, *decision);
        let snapshot = load_group(&pool, group_id).await.unwrap().unwrap();
        assert_eq!(snapshot.pending, 1, "a replay must not double-count");
        assert_eq!(snapshot.actor_count, 1);

        let (first_event_id, first_actor, first_decision) = &events[0];
        let first_replay = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            first_event_id,
            first_actor,
        )
        .await;
        assert_eq!(first_replay, *first_decision);
        let stored = stored_disposition(&pool, first_event_id, &owner)
            .await
            .expect("disposition stored");
        assert!(stored.starts_with('i'));

        for (_, _, decision) in &events {
            if let CoalesceDecision::Buffered { group_id } = decision {
                cleanup_group(&pool, group_id).await;
            }
        }
    }

    #[tokio::test]
    async fn throttle_demotes_without_consuming_an_immediate_slot() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 3;
        settings.recipient_throttle_capacity = 1;
        settings.recipient_throttle_refill_secs = 3600;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let first = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        assert!(matches!(first, CoalesceDecision::Immediate { .. }));

        let second_event = Uuid::new_v4().to_string();
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &second_event,
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = &second else {
            panic!("an empty token bucket must demote the second push");
        };

        let snapshot = load_group(&pool, group_id).await.unwrap().unwrap();
        assert_eq!(
            snapshot.pending, 1,
            "the throttled push is buffered, not dropped"
        );
        assert_eq!(
            snapshot.immediate, 1,
            "a throttled push must not spend an immediate slot"
        );

        // Replaying the throttled event must not double-count the buffer.
        let replay = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &second_event,
            "unused-on-replay",
        )
        .await;
        assert_eq!(replay, second);
        let snapshot = load_group(&pool, group_id).await.unwrap().unwrap();
        assert_eq!(snapshot.pending, 1);

        cleanup_group(&pool, group_id).await;
    }

    #[tokio::test]
    async fn refill_restores_an_immediate_slot() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 3;
        settings.recipient_throttle_capacity = 1;
        settings.recipient_throttle_refill_secs = 1;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let first = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        assert!(matches!(first, CoalesceDecision::Immediate { .. }));
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        assert!(matches!(second, CoalesceDecision::Buffered { .. }));

        tokio::time::sleep(Duration::from_millis(1100)).await;

        let third = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        assert!(
            matches!(third, CoalesceDecision::Immediate { .. }),
            "a refilled bucket must allow an immediate push"
        );

        if let CoalesceDecision::Immediate { group_id, .. } = &third {
            cleanup_group(&pool, group_id).await;
        }
    }

    #[tokio::test]
    async fn bucket_rollover_starts_a_new_group_and_replays_stably() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_window_secs = 1;
        settings.coalesce_immediate_limit = 1;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let first_event = Uuid::new_v4().to_string();
        let first = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &first_event,
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Immediate {
            group_id: first_gid,
            ..
        } = &first
        else {
            panic!("the first interaction must send immediately");
        };

        tokio::time::sleep(Duration::from_millis(1200)).await;

        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Immediate {
            group_id: second_gid,
            ..
        } = &second
        else {
            panic!("a new bucket resets the immediate budget");
        };
        assert_ne!(first_gid, second_gid, "a new bucket is a new group");

        let replay = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &first_event,
            "unused-on-replay",
        )
        .await;
        assert_eq!(
            replay, first,
            "a replay keeps the original bucket's decision"
        );

        cleanup_group(&pool, first_gid).await;
        cleanup_group(&pool, second_gid).await;
    }

    #[tokio::test]
    async fn expired_lease_is_recovered_without_losing_pending_work() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 1;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let first = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        assert!(matches!(first, CoalesceDecision::Immediate { .. }));
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;

        let claim = claim_due_group(&pool, 1)
            .await
            .unwrap()
            .expect("the group is claimable");
        assert_eq!(claim.group_id, group_id);

        // Crash after the claim: the lease expires with the work unfinished.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let recovered = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("an expired lease with pending work must be reclaimed");
        assert_eq!(
            recovered.group_id, group_id,
            "recovery must not lose the buffered group"
        );
        assert_eq!(
            complete_group(&pool, &recovered, None).await.unwrap(),
            CompleteOutcome::Completed
        );
        assert!(load_group(&pool, &group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_group_is_deleted_not_requeued() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 3;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let mut group_id = None;
        for _ in 0..3 {
            let decision = ingest_one(
                &pool,
                &settings,
                &owner,
                &target,
                &Uuid::new_v4().to_string(),
                &random_actor(),
            )
            .await;
            let CoalesceDecision::Immediate { group_id: gid, .. } = decision else {
                panic!("all three interactions fit the immediate budget");
            };
            group_id = Some(gid);
        }
        let group_id = group_id.unwrap();

        // Simulate a stale lease on a group whose events were all immediate.
        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZADD")
            .arg(LEASES_KEY)
            .arg(Timestamp::now().as_secs().saturating_sub(10))
            .arg(&group_id)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        assert!(
            claim_due_group(&pool, 30).await.unwrap().is_none(),
            "a group with nothing pending is not work"
        );
        assert!(
            load_group(&pool, &group_id).await.unwrap().is_none(),
            "reconciliation must delete an empty group, not requeue it"
        );
    }

    #[tokio::test]
    async fn completion_requires_the_lease_token() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 1;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;

        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let impostor = GroupClaim {
            group_id: group_id.clone(),
            token: "not-the-lease".to_string(),
            claimed_at: claim.claimed_at,
        };
        assert_eq!(
            complete_group(&pool, &impostor, None).await.unwrap(),
            CompleteOutcome::NotOwner,
            "a non-owner completion must be refused"
        );
        assert!(
            load_group(&pool, &group_id).await.unwrap().is_some(),
            "a refused completion must leave the group intact"
        );

        assert_eq!(
            complete_group(&pool, &claim, None).await.unwrap(),
            CompleteOutcome::Completed
        );
        assert!(load_group(&pool, &group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dangling_due_member_is_dropped() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let gid = format!("like:{}:e:{}:1", "a".repeat(64), Uuid::new_v4());

        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZADD")
            .arg(DUE_KEY)
            .arg(Timestamp::now().as_secs())
            .arg(&gid)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
        let score: Option<u64> = redis::cmd("ZSCORE")
            .arg(DUE_KEY)
            .arg(&gid)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(score.is_some());
        drop(conn);

        assert!(claim_due_group(&pool, 30).await.unwrap().is_none());

        let mut conn = pool.get().await.unwrap();
        let score: Option<u64> = redis::cmd("ZSCORE")
            .arg(DUE_KEY)
            .arg(&gid)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(score.is_none(), "a dangling member must be removed");
    }

    /// Overwrite the per-recipient throttle bucket for a test. A `ts` in the
    /// future makes the lazy refill clamp to zero elapsed time.
    async fn set_throttle_state(pool: &RedisPool, owner: &PublicKey, tokens: f64, ts_ms: u64) {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("HSET")
            .arg(format!("{THROTTLE_PREFIX}{}", owner.to_hex()))
            .arg("tokens")
            .arg(tokens)
            .arg("ts")
            .arg(ts_ms)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
    }

    /// Overwrite the rolling emission window for a test, with absolute Redis
    /// server clock scores so a test can place emissions inside or outside it.
    async fn set_emitted_window(pool: &RedisPool, owner: &PublicKey, entries: &[(u64, &str)]) {
        let mut conn = pool.get().await.unwrap();
        let key = format!("{EMITTED_PREFIX}{}", owner.to_hex());
        redis::cmd("DEL")
            .arg(&key)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
        let mut cmd = redis::cmd("ZADD");
        cmd.arg(&key);
        for (score, member) in entries {
            cmd.arg(score).arg(member);
        }
        cmd.query_async::<i64>(&mut *conn).await.unwrap();
    }

    fn never_panicking_sender() -> FcmClient {
        FcmClient::new_with_impl(Box::new(PanickingFcmSender {
            panic_on: "a token this test never registers".to_string(),
        }))
    }

    #[tokio::test]
    async fn buffered_groups_carry_the_logical_lifetime() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 1;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };

        let now = Timestamp::now().as_secs();
        let snapshot = load_group(&pool, &group_id).await.unwrap().unwrap();
        assert!(
            snapshot.expires_at >= now + settings.coalesce_group_ttl_secs - 5
                && snapshot.expires_at <= now + settings.coalesce_group_ttl_secs + 5,
            "a buffered group must carry its logical expiry, got {}",
            snapshot.expires_at
        );

        let physical = settings
            .coalesce_group_ttl_secs
            .saturating_add(settings.coalesce_logical_expiry_grace_secs);
        let window_plus_grace = settings
            .coalesce_window_secs
            .saturating_add(settings.coalesce_logical_expiry_grace_secs);
        let mut conn = pool.get().await.unwrap();
        let group_ttl: i64 = redis::cmd("TTL")
            .arg(format!("{GROUP_PREFIX}{group_id}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        let hll_ttl: i64 = redis::cmd("TTL")
            .arg(format!("{HLL_PREFIX}{group_id}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        assert!(
            group_ttl > window_plus_grace as i64,
            "the physical TTL must outlive the window plus grace so a backlog cannot silently expire it, got {group_ttl}"
        );
        assert!(
            group_ttl <= physical as i64 && group_ttl > physical as i64 - 10,
            "the physical TTL is the logical lifetime plus grace, got {group_ttl}"
        );
        assert!(hll_ttl > 0, "the actor counter must share the lifetime");

        cleanup_group(&pool, &group_id).await;
    }

    #[tokio::test]
    async fn flush_drops_a_group_that_reached_its_logical_lifetime() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.allowed_pubkeys.clear();
        settings.service.coalesce_immediate_limit = 1;
        settings.service.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;
        {
            let mut conn = pool.get().await.unwrap();
            redis::cmd("HSET")
                .arg(format!("{GROUP_PREFIX}{group_id}"))
                .arg("expires_at")
                .arg(Timestamp::now().as_secs().saturating_sub(1))
                .query_async::<i64>(&mut *conn)
                .await
                .unwrap();
        }

        let state = test_state(settings, pool.clone(), never_panicking_sender());
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Expired,
            "an expired group is dropped, not sent"
        );
        assert!(
            load_group(&pool, &group_id).await.unwrap().is_none(),
            "the expired group must be deleted while it is still readable"
        );
    }

    #[tokio::test]
    async fn requeue_across_the_logical_lifetime_drops_instead_of_requeueing() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 1;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;

        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        assert_eq!(
            complete_group(&pool, &claim, Some(5)).await.unwrap(),
            CompleteOutcome::Requeued,
            "a retry that fits inside the lifetime returns to the due queue"
        );

        make_claimable(&pool, &group_id).await;
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the requeued group is claimable");
        {
            let mut conn = pool.get().await.unwrap();
            redis::cmd("HSET")
                .arg(format!("{GROUP_PREFIX}{group_id}"))
                .arg("expires_at")
                .arg(Timestamp::now().as_secs().saturating_add(1))
                .query_async::<i64>(&mut *conn)
                .await
                .unwrap();
        }
        assert_eq!(
            complete_group(&pool, &claim, Some(60)).await.unwrap(),
            CompleteOutcome::Expired,
            "a retry that would cross the lifetime is dropped instead of requeued"
        );
        assert!(load_group(&pool, &group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_summary_throttled_at_flush_defers_to_the_refill_time() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.allowed_pubkeys.clear();
        settings.service.coalesce_immediate_limit = 1;
        settings.service.recipient_throttle_capacity = 100;
        settings.service.recipient_throttle_refill_secs = 60;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;
        redis_store::add_or_update_token(&pool, &owner, "coalesce-throttle-token")
            .await
            .unwrap();

        // A `ts` in the future makes the lazy refill see zero elapsed time.
        set_throttle_state(
            &pool,
            &owner,
            0.0,
            (Timestamp::now().as_secs() * 1000) + 60_000,
        )
        .await;

        let state = test_state(settings, pool.clone(), never_panicking_sender());
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Throttled,
            "an empty recipient bucket defers the summary rather than sending it"
        );
        assert!(
            load_group(&pool, &group_id).await.unwrap().is_some(),
            "the deferred group must survive"
        );

        let mut conn = pool.get().await.unwrap();
        let score: Option<u64> = redis::cmd("ZSCORE")
            .arg(DUE_KEY)
            .arg(&group_id)
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        let now = Timestamp::now().as_secs();
        assert!(
            score.is_some_and(|score| score >= now + 55 && score <= now + 70),
            "the group must return at the refill time, got {score:?}"
        );

        // A refilled bucket lets the same group send.
        set_throttle_state(
            &pool,
            &owner,
            1.0,
            (Timestamp::now().as_secs() * 1000) + 60_000,
        )
        .await;
        {
            let mut conn = pool.get().await.unwrap();
            redis::cmd("ZADD")
                .arg(DUE_KEY)
                .arg(Timestamp::now().as_secs())
                .arg(&group_id)
                .query_async::<i64>(&mut *conn)
                .await
                .unwrap();
        }
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the refilled bucket group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Sent {
                delivered: 1,
                failed: 0,
                actor_count: 1,
            }
        );

        redis_store::remove_token(&pool, &owner, "coalesce-throttle-token")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_recipient_at_the_daily_cap_defers_until_the_window_slides() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.allowed_pubkeys.clear();
        settings.service.coalesce_immediate_limit = 3;
        // The bucket is deliberately not the binding bound here: the rolling
        // emission window is, and its slide (an hour) must be far from the
        // token refill so the requeue delay tells the two bounds apart.
        settings.service.recipient_throttle_capacity = 100;
        settings.service.recipient_throttle_refill_secs = 60;
        settings.service.recipient_daily_window_secs = 3600;
        settings.service.recipient_daily_cap = 2;
        let owner = Keys::generate().public_key();
        let target = random_target();

        for _ in 0..2 {
            let decision = ingest_one(
                &pool,
                &settings.service,
                &owner,
                &target,
                &Uuid::new_v4().to_string(),
                &random_actor(),
            )
            .await;
            assert!(
                matches!(decision, CoalesceDecision::Immediate { .. }),
                "the first cap-many events send immediately"
            );
        }

        let third = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = &third else {
            panic!("an event over the daily cap must buffer, not send");
        };
        let snapshot = load_group(&pool, group_id).await.unwrap().unwrap();
        assert_eq!(
            snapshot.pending, 1,
            "the capped event is buffered, not dropped"
        );
        assert_eq!(
            snapshot.immediate, 2,
            "a capped event must not spend an immediate slot"
        );

        // A summary for the same recipient is deferred, not dropped, while the
        // group is inside its logical lifetime.
        redis_store::add_or_update_token(&pool, &owner, "coalesce-daily-cap-token")
            .await
            .unwrap();
        make_claimable(&pool, group_id).await;
        let state = test_state(settings.clone(), pool.clone(), never_panicking_sender());
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Throttled,
            "a summary at the daily cap must defer rather than send"
        );
        assert!(
            load_group(&pool, group_id).await.unwrap().is_some(),
            "a deferred summary must survive until its logical expiry or the slide"
        );

        // The requeue waits for the window to slide, not for the token refill.
        let now = Timestamp::now().as_secs();
        let window = settings.service.recipient_daily_window_secs;
        let mut conn = pool.get().await.unwrap();
        let score: Option<u64> = redis::cmd("ZSCORE")
            .arg(DUE_KEY)
            .arg(group_id)
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        assert!(
            score.is_some_and(|score| score >= now + window - 10 && score <= now + window + 5),
            "a capped group must return when the oldest emission ages out, got {score:?}"
        );

        // Age the emissions out of the window; the deferred summary now sends.
        set_emitted_window(
            &pool,
            &owner,
            &[
                (now - window - 10, "aged-out-1"),
                (now - window - 5, "aged-out-2"),
            ],
        )
        .await;
        make_claimable(&pool, group_id).await;
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the requeued group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Sent {
                delivered: 1,
                failed: 0,
                actor_count: 1,
            },
            "the window sliding must admit the deferred summary"
        );

        redis_store::remove_token(&pool, &owner, "coalesce-daily-cap-token")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_duplicate_spend_of_one_group_counts_once_in_the_emission_window() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.recipient_throttle_capacity = 5;
        settings.recipient_daily_cap = 3;
        settings.recipient_daily_window_secs = 3600;
        let owner = Keys::generate().public_key();
        let group_id = format!("like:{}:e:{}:0", owner.to_hex(), Uuid::new_v4());

        // A lease-recovered flush can spend the same group's budget twice; the
        // window member is the group id, so the emission must count once.
        for _ in 0..2 {
            let outcome = consume_recipient_token(&pool, &owner.to_hex(), &group_id, &settings)
                .await
                .unwrap();
            assert_eq!(outcome, SpendOutcome::Spent);
        }

        let mut conn = pool.get().await.unwrap();
        let emitted: i64 = redis::cmd("ZCARD")
            .arg(format!("{EMITTED_PREFIX}{}", owner.to_hex()))
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert_eq!(
            emitted, 1,
            "a re-spend of the same emission must not inflate the rolling count"
        );
    }

    #[tokio::test]
    async fn a_recovered_group_reuses_its_reservation_at_the_daily_cap() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.recipient_throttle_capacity = 2;
        settings.recipient_throttle_refill_secs = 3600;
        settings.recipient_daily_cap = 1;
        settings.recipient_daily_window_secs = 3600;
        let owner = Keys::generate().public_key();
        let group_id = format!("like:{}:e:{}:0", owner.to_hex(), Uuid::new_v4());

        assert_eq!(
            consume_recipient_token(&pool, &owner.to_hex(), &group_id, &settings)
                .await
                .unwrap(),
            SpendOutcome::Spent
        );
        assert_eq!(
            consume_recipient_token(&pool, &owner.to_hex(), &group_id, &settings)
                .await
                .unwrap(),
            SpendOutcome::Spent,
            "lease recovery must not be blocked by the group's own reservation"
        );

        let mut conn = pool.get().await.unwrap();
        let tokens: f64 = redis::cmd("HGET")
            .arg(format!("{THROTTLE_PREFIX}{}", owner.to_hex()))
            .arg("tokens")
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert_eq!(tokens, 1.0, "recovery must not spend a second token");
    }

    struct RetryableFcmSender {
        delay: Duration,
    }

    #[async_trait]
    impl FcmSend for RetryableFcmSender {
        async fn send_single(
            &self,
            _token: &str,
            _payload: FcmPayload,
        ) -> std::result::Result<(), FcmError> {
            Err(FcmError::RetryableInternal(self.delay))
        }
    }

    struct AllInvalidFcmSender;

    #[async_trait]
    impl FcmSend for AllInvalidFcmSender {
        async fn send_single(
            &self,
            _token: &str,
            _payload: FcmPayload,
        ) -> std::result::Result<(), FcmError> {
            Err(FcmError::TokenNotRegistered)
        }
    }

    #[tokio::test]
    async fn a_terminal_all_invalid_summary_failure_completes_and_clears_the_window() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.allowed_pubkeys.clear();
        settings.service.coalesce_immediate_limit = 1;
        settings.service.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;
        redis_store::add_or_update_token(&pool, &owner, "coalesce-invalid-token")
            .await
            .unwrap();

        let state = test_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(AllInvalidFcmSender)),
        );
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Sent {
                delivered: 0,
                failed: 1,
                actor_count: 1,
            },
            "every token failing non-retryably is a terminal outcome"
        );
        assert!(
            load_group(&pool, &group_id).await.unwrap().is_none(),
            "a terminal failure must complete the group"
        );

        // Nothing was emitted, so the spent token and window slot come back.
        let mut conn = pool.get().await.unwrap();
        let emitted: Option<u64> = redis::cmd("ZSCORE")
            .arg(format!("{EMITTED_PREFIX}{}", owner.to_hex()))
            .arg(&group_id)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(
            emitted.is_none(),
            "a terminal failure that emitted nothing must leave the rolling window"
        );
        drop(conn);
        redis_store::remove_token(&pool, &owner, "coalesce-invalid-token")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn immediate_only_groups_keep_the_short_ttl() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 3;
        settings.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let decision = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Immediate { group_id, .. } = decision else {
            panic!("the first interaction must send immediately");
        };

        let short = settings
            .coalesce_window_secs
            .saturating_add(settings.coalesce_logical_expiry_grace_secs);
        let physical = settings
            .coalesce_group_ttl_secs
            .saturating_add(settings.coalesce_logical_expiry_grace_secs);
        let mut conn = pool.get().await.unwrap();
        let ttl: i64 = redis::cmd("TTL")
            .arg(format!("{GROUP_PREFIX}{group_id}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        assert!(
            ttl > 0 && ttl <= short as i64 + 2,
            "an immediate-only group only needs to outlive its bucket, got {ttl}"
        );
        assert!(
            ttl < physical as i64,
            "an immediate-only group must not hold the buffered lifetime"
        );

        cleanup_group(&pool, &group_id).await;
    }

    #[tokio::test]
    async fn an_immediate_event_does_not_shorten_a_buffered_group() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = test_settings();
        settings.coalesce_immediate_limit = 3;
        settings.recipient_throttle_capacity = 1;
        settings.recipient_throttle_refill_secs = 3600;
        let owner = Keys::generate().public_key();
        let target = random_target();

        // The first interaction sends immediately and consumes the only token.
        let first = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        assert!(matches!(first, CoalesceDecision::Immediate { .. }));

        // The second is throttled into the bucket and makes the group durable.
        let second = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the throttled interaction must buffer");
        };

        // A refilled bucket lets a third interaction send immediately on the
        // same buffered group; it must not shorten the group's lifetime.
        set_throttle_state(
            &pool,
            &owner,
            1.0,
            (Timestamp::now().as_secs() * 1000) + 60_000,
        )
        .await;
        let third = ingest_one(
            &pool,
            &settings,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Immediate {
            group_id: third_gid,
            ..
        } = third
        else {
            panic!("a refilled bucket must allow an immediate push");
        };
        assert_eq!(
            third_gid, group_id,
            "the immediate event joins the same group"
        );

        let window_plus_grace = settings
            .coalesce_window_secs
            .saturating_add(settings.coalesce_logical_expiry_grace_secs);
        let mut conn = pool.get().await.unwrap();
        let ttl: i64 = redis::cmd("TTL")
            .arg(format!("{GROUP_PREFIX}{group_id}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        assert!(
            ttl > window_plus_grace as i64,
            "an immediate event must not cut a buffered group back to the short TTL, got {ttl}"
        );
        let snapshot = load_group(&pool, &group_id).await.unwrap().unwrap();
        assert_eq!(snapshot.pending, 1, "the buffered work must survive");

        cleanup_group(&pool, &group_id).await;
    }

    #[tokio::test]
    async fn a_retryable_summary_failure_refunds_the_token_and_honours_retry_after() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.allowed_pubkeys.clear();
        settings.service.coalesce_immediate_limit = 1;
        settings.service.recipient_throttle_capacity = 100;
        settings.service.recipient_throttle_refill_secs = 60;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;
        redis_store::add_or_update_token(&pool, &owner, "coalesce-retry-token")
            .await
            .unwrap();

        // Exactly one token, with no refill during the test.
        set_throttle_state(
            &pool,
            &owner,
            1.0,
            (Timestamp::now().as_secs() * 1000) + 60_000,
        )
        .await;

        let state = test_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(RetryableFcmSender {
                delay: Duration::from_secs(600),
            })),
        );
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::RetryQueued,
            "an all-token retryable failure must requeue the group"
        );

        let mut conn = pool.get().await.unwrap();
        let tokens: f64 = redis::cmd("HGET")
            .arg(format!("{THROTTLE_PREFIX}{}", owner.to_hex()))
            .arg("tokens")
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(
            tokens >= 1.0,
            "a flush that emitted nothing must refund its token, got {tokens}"
        );
        let emitted: Option<u64> = redis::cmd("ZSCORE")
            .arg(format!("{EMITTED_PREFIX}{}", owner.to_hex()))
            .arg(&group_id)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(
            emitted.is_none(),
            "a flush that emitted nothing must leave the rolling emission window"
        );

        let score: Option<u64> = redis::cmd("ZSCORE")
            .arg(DUE_KEY)
            .arg(&group_id)
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        let now = Timestamp::now().as_secs();
        assert!(
            score.is_some_and(|score| score >= now + 590 && score <= now + 610),
            "the requeue must honour FCM's Retry-After rather than the 5 s floor, got {score:?}"
        );

        redis_store::remove_token(&pool, &owner, "coalesce-retry-token")
            .await
            .unwrap();
        cleanup_group(&pool, &group_id).await;
    }

    #[tokio::test]
    async fn oldest_due_age_tracks_the_head_of_the_queue() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        assert_eq!(
            oldest_due_age_seconds(&pool).await.unwrap(),
            0,
            "an empty queue has no age"
        );

        let gid = format!("like:{}:e:{}:1", "a".repeat(64), Uuid::new_v4());
        let score = Timestamp::now().as_secs().saturating_sub(100);
        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZADD")
            .arg(DUE_KEY)
            .arg(score)
            .arg(&gid)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        let age = oldest_due_age_seconds(&pool).await.unwrap();
        assert!(
            age >= 100,
            "the gauge must report the backdated head, got {age}"
        );
        cleanup_group(&pool, &gid).await;
    }

    struct PanickingFcmSender {
        panic_on: String,
    }

    #[async_trait]
    impl FcmSend for PanickingFcmSender {
        async fn send_single(
            &self,
            token: &str,
            _payload: FcmPayload,
        ) -> std::result::Result<(), FcmError> {
            if token == self.panic_on {
                panic!("simulated upstream panic while sending a summary");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_panicking_send_is_contained_and_the_group_still_completes() {
        let _guard = test_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        reset_indexes(&pool).await;
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.coalesce_immediate_limit = 1;
        settings.service.recipient_throttle_capacity = 100;
        let owner = Keys::generate().public_key();
        let target = random_target();

        let _ = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let second = ingest_one(
            &pool,
            &settings.service,
            &owner,
            &target,
            &Uuid::new_v4().to_string(),
            &random_actor(),
        )
        .await;
        let CoalesceDecision::Buffered { group_id } = second else {
            panic!("the second interaction must buffer");
        };
        make_claimable(&pool, &group_id).await;

        redis_store::add_or_update_token(&pool, &owner, "coalesce-panic-good")
            .await
            .unwrap();
        redis_store::add_or_update_token(&pool, &owner, "coalesce-panic-bad")
            .await
            .unwrap();

        let state = test_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(PanickingFcmSender {
                panic_on: "coalesce-panic-bad".to_string(),
            })),
        );
        let claim = claim_due_group(&pool, 30)
            .await
            .unwrap()
            .expect("the group is claimable");
        let outcome = flush_group(&state, &claim, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            FlushOutcome::Sent {
                delivered: 1,
                failed: 1,
                actor_count: 1,
            },
            "a panic on one token must not take the group down"
        );
        assert!(
            load_group(&pool, &group_id).await.unwrap().is_none(),
            "the group must complete despite the contained panic"
        );

        redis_store::remove_token(&pool, &owner, "coalesce-panic-good")
            .await
            .unwrap();
        redis_store::remove_token(&pool, &owner, "coalesce-panic-bad")
            .await
            .unwrap();
    }
}
