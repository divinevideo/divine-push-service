//! Redis storage for diVine Push Service
//!
//! Simplified Redis storage without multi-app namespacing.
//! Stores:
//! - User device tokens
//! - Processed event tracking
//! - Profile cache
//! - User preferences (handled by preferences module)

use crate::error::{Result, ServiceError};
use bb8_redis::bb8::Pool;
use bb8_redis::RedisConnectionManager;
use nostr_sdk::{EventId, PublicKey, Timestamp};
use redis::{RedisResult, Value};
use std::collections::HashSet;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// Type alias for the connection pool
pub type RedisPool = Pool<RedisConnectionManager>;

// Redis key constants
const STALE_TOKENS_ZSET: &str = "stale_tokens";
const TOKEN_TO_PUBKEY_HASH: &str = "token_to_pubkey";
const NEW_POST_FANOUT_JOBS_ZSET: &str = "new_post_fanout_jobs";

/// Build key for user tokens set
fn build_user_tokens_key(pubkey: &PublicKey) -> String {
    format!("user_tokens:{}", pubkey.to_hex())
}

/// Creates a new Redis connection pool.
pub async fn create_pool(redis_url: &str, pool_size: u32) -> Result<RedisPool> {
    let manager = RedisConnectionManager::new(redis_url).map_err(ServiceError::Redis)?;
    Pool::builder()
        .max_size(pool_size)
        .connection_timeout(Duration::from_secs(15))
        .build(manager)
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to build Redis pool: {}", e)))
}

// =============================================================================
// Token Management
// =============================================================================

/// Retrieves device tokens associated with a public key
pub async fn get_tokens_for_pubkey(pool: &RedisPool, pubkey: &PublicKey) -> Result<Vec<String>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let user_tokens_key = build_user_tokens_key(pubkey);

    let tokens: Vec<String> = redis::cmd("SMEMBERS")
        .arg(&user_tokens_key)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(tokens)
}

/// Adds or updates a single device token for a pubkey
pub async fn add_or_update_token(pool: &RedisPool, pubkey: &PublicKey, token: &str) -> Result<()> {
    let now_timestamp = Timestamp::now().as_secs();
    let pubkey_hex = pubkey.to_hex();
    let user_tokens_key = build_user_tokens_key(pubkey);

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    // Check if this token is already associated with a different pubkey
    let existing_pubkey: Option<String> = redis::cmd("HGET")
        .arg(TOKEN_TO_PUBKEY_HASH)
        .arg(token)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    // Use a pipeline to ensure atomicity
    let mut pipe = redis::pipe();
    pipe.atomic();

    // If token was previously registered to a different pubkey, clean up old association
    if let Some(existing) = existing_pubkey {
        if existing != pubkey_hex {
            tracing::warn!(
                "Token re-registration: Token moving from pubkey {} to {}",
                existing,
                pubkey_hex
            );
            // Parse the existing pubkey and remove token from old user's set
            if let Ok(old_pubkey) = PublicKey::from_hex(&existing) {
                let old_user_tokens_key = build_user_tokens_key(&old_pubkey);
                pipe.srem(&old_user_tokens_key, token);
            }
        }
    }

    // Add/update token for new pubkey
    pipe.sadd(&user_tokens_key, token) // Add token to user's set
        .zadd(STALE_TOKENS_ZSET, token, now_timestamp) // Track for cleanup
        .hset(TOKEN_TO_PUBKEY_HASH, token, &pubkey_hex); // Map token back to pubkey

    let _result = pipe
        .query_async::<Value>(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(())
}

/// Removes a single device token for a pubkey.
/// Returns true if the token was found and removed.
/// Only allows removal if the token is registered to the requesting pubkey.
pub async fn remove_token(pool: &RedisPool, pubkey: &PublicKey, token: &str) -> Result<bool> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    // First check if this token belongs to the requesting pubkey
    let token_owner: Option<String> = redis::cmd("HGET")
        .arg(TOKEN_TO_PUBKEY_HASH)
        .arg(token)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    let pubkey_hex = pubkey.to_hex();

    match token_owner {
        Some(owner) if owner == pubkey_hex => {
            // Token belongs to this pubkey, proceed with removal
            let user_tokens_key = build_user_tokens_key(pubkey);

            let mut pipe = redis::pipe();
            pipe.atomic()
                .srem(&user_tokens_key, token) // Remove from user's set
                .zrem(STALE_TOKENS_ZSET, token) // Remove from stale tracking
                .hdel(TOKEN_TO_PUBKEY_HASH, token); // Remove from token->pubkey map

            let _result: RedisResult<(usize, usize, usize)> = pipe.query_async(&mut *conn).await;
            _result.map(|_| ()).map_err(ServiceError::Redis)?;
            Ok(true)
        }
        Some(owner) => {
            tracing::warn!(
                "Deregistration rejected! Token owned by {} but deregistration attempted by {}",
                owner,
                pubkey_hex
            );
            Ok(false) // Return false to indicate token wasn't removed
        }
        None => {
            // Token doesn't exist
            Ok(false)
        }
    }
}

/// Cleans up stale tokens based on their last_seen timestamp.
pub async fn cleanup_stale_tokens(pool: &RedisPool, max_age_seconds: i64) -> Result<usize> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let now_timestamp = Timestamp::now().as_secs();
    let cutoff_timestamp = now_timestamp.saturating_sub(max_age_seconds as u64);

    // 1. Find tokens with score <= cutoff_timestamp
    let stale_tokens: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(STALE_TOKENS_ZSET)
        .arg("-inf")
        .arg(cutoff_timestamp)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    if stale_tokens.is_empty() {
        tracing::debug!("No stale tokens found to clean up.");
        return Ok(0);
    }

    let count = stale_tokens.len();
    tracing::info!("Found {} stale tokens to clean up.", count);

    // 2. Get the associated pubkeys for the stale tokens
    let pubkeys_hex: Vec<Option<String>> = redis::cmd("HMGET")
        .arg(TOKEN_TO_PUBKEY_HASH)
        .arg(&stale_tokens)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    // 3. Start a pipeline for atomic removal
    let mut pipe = redis::pipe();
    pipe.atomic();

    // 3a. Remove tokens from the ZSET
    let mut zrem_cmd = redis::cmd("ZREMRANGEBYSCORE");
    zrem_cmd
        .arg(STALE_TOKENS_ZSET)
        .arg("-inf")
        .arg(cutoff_timestamp);
    pipe.add_command(zrem_cmd);

    // 3b. Remove tokens from the HASH
    pipe.hdel(TOKEN_TO_PUBKEY_HASH, &stale_tokens);

    // 3c. Remove tokens from individual user sets
    let mut actual_removed_count = 0;
    for (token, pubkey_hex_opt) in stale_tokens.iter().zip(pubkeys_hex.iter()) {
        if let Some(pubkey_hex) = pubkey_hex_opt {
            let user_tokens_key = format!("user_tokens:{}", pubkey_hex);
            pipe.srem(user_tokens_key, token);
            actual_removed_count += 1;
        } else {
            tracing::warn!(
                "Pubkey not found in hash for stale token: {}. Skipping user set removal.",
                token
            );
        }
    }

    // 4. Execute the pipeline
    let _result: RedisResult<Value> = pipe.query_async(&mut *conn).await;

    match _result {
        Ok(_) => {
            tracing::info!(
                "Successfully cleaned up {} stale tokens (attempted removal from {} user sets).",
                count,
                actual_removed_count
            );
            Ok(count)
        }
        Err(e) => {
            tracing::error!("Error during stale token cleanup pipeline: {}", e);
            Err(ServiceError::Redis(e))
        }
    }
}

// =============================================================================
// Event Processing
// =============================================================================

/// Atomically claims an event for processing using SET NX EX.
/// Returns true if the event was newly claimed (not yet processed).
/// Returns false if the event was already claimed by another replica.
pub async fn try_claim_event(
    pool: &RedisPool,
    event_id: &EventId,
    ttl_seconds: u64,
) -> Result<bool> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let key = format!("dedup:{}", event_id.to_hex());

    let result: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(ttl_seconds)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    // SET NX returns "OK" if the key was set, None if it already existed
    Ok(result.is_some())
}

/// Ownership token for a per-recipient event claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientEventClaim {
    key: String,
    value: String,
}

/// Claims one `(event_id, recipient)` delivery before it reaches FCM.
pub async fn try_claim_recipient_event(
    pool: &RedisPool,
    event_id: &EventId,
    recipient: &PublicKey,
    ttl_seconds: u64,
) -> Result<Option<RecipientEventClaim>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let claim = RecipientEventClaim {
        key: format!("dedup:{}:{}", event_id.to_hex(), recipient.to_hex()),
        value: Uuid::new_v4().to_string(),
    };
    let result: Option<String> = redis::cmd("SET")
        .arg(&claim.key)
        .arg(&claim.value)
        .arg("NX")
        .arg("EX")
        .arg(ttl_seconds)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(result.map(|_| claim))
}

/// Releases a claim only if it is still owned by this delivery attempt.
pub async fn release_recipient_event_claim(
    pool: &RedisPool,
    claim: &RecipientEventClaim,
) -> Result<bool> {
    const RELEASE_SCRIPT: &str = r#"
        if redis.call('GET', KEYS[1]) == ARGV[1] then
          return redis.call('DEL', KEYS[1])
        end
        return 0
    "#;
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let released: i64 = redis::Script::new(RELEASE_SCRIPT)
        .key(&claim.key)
        .arg(&claim.value)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(released == 1)
}

fn unix_time_millis() -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ServiceError::Internal(format!("System clock is before Unix epoch: {}", e)))?;
    u64::try_from(duration.as_millis())
        .map_err(|_| ServiceError::Internal("Unix timestamp does not fit in u64".to_string()))
}

/// Atomically records an event's initial durable fan-out job once.
pub async fn enqueue_initial_fanout_job(
    pool: &RedisPool,
    event_id: &EventId,
    job: &str,
    marker_ttl_secs: u64,
) -> Result<bool> {
    const ENQUEUE_SCRIPT: &str = r#"
        if redis.call('EXISTS', KEYS[1]) == 1 then
          return 0
        end
        redis.call('ZADD', KEYS[2], ARGV[2], ARGV[3])
        redis.call('SET', KEYS[1], '1', 'EX', ARGV[1])
        return 1
    "#;
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let enqueued: i64 = redis::Script::new(ENQUEUE_SCRIPT)
        .key(format!("fanout:enqueued:{}", event_id.to_hex()))
        .key(NEW_POST_FANOUT_JOBS_ZSET)
        .arg(marker_ttl_secs)
        .arg(unix_time_millis()?)
        .arg(job)
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(enqueued == 1)
}

/// Claims the oldest available fan-out job until its lease expires.
pub async fn claim_fanout_job(pool: &RedisPool, lease_secs: u64) -> Result<Option<String>> {
    const CLAIM_SCRIPT: &str = r#"
        local jobs = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, 1)
        if #jobs == 0 then
          return nil
        end
        redis.call('ZADD', KEYS[1], ARGV[2], jobs[1])
        return jobs[1]
    "#;
    let now = unix_time_millis()?;
    let lease_millis = lease_secs.saturating_mul(1000);
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    redis::Script::new(CLAIM_SCRIPT)
        .key(NEW_POST_FANOUT_JOBS_ZSET)
        .arg(now)
        .arg(now.saturating_add(lease_millis))
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)
}

/// Completes one page and atomically schedules its successor, if any.
pub async fn complete_fanout_job(
    pool: &RedisPool,
    current_job: &str,
    next_job: Option<&str>,
) -> Result<()> {
    const COMPLETE_SCRIPT: &str = r#"
        if ARGV[2] ~= '' then
          redis.call('ZADD', KEYS[1], 'NX', ARGV[3], ARGV[2])
        end
        redis.call('ZREM', KEYS[1], ARGV[1])
        return 1
    "#;
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    redis::Script::new(COMPLETE_SCRIPT)
        .key(NEW_POST_FANOUT_JOBS_ZSET)
        .arg(current_job)
        .arg(next_job.unwrap_or_default())
        .arg(unix_time_millis()?)
        .invoke_async::<i64>(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(())
}

/// Makes the same leased fan-out job available again after a bounded delay.
pub async fn rescore_fanout_job(pool: &RedisPool, job: &str, delay_secs: u64) -> Result<()> {
    let available_at = unix_time_millis()?.saturating_add(delay_secs.saturating_mul(1000));
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    redis::cmd("ZADD")
        .arg(NEW_POST_FANOUT_JOBS_ZSET)
        .arg("XX")
        .arg(available_at)
        .arg(job)
        .query_async::<()>(&mut *conn)
        .await
        .map_err(ServiceError::Redis)
}

/// Atomically replaces a failed page with its next retry attempt.
pub async fn retry_fanout_job(
    pool: &RedisPool,
    current_job: &str,
    retry_job: &str,
    delay_secs: u64,
) -> Result<()> {
    const RETRY_SCRIPT: &str = r#"
        redis.call('ZADD', KEYS[1], ARGV[3], ARGV[2])
        redis.call('ZREM', KEYS[1], ARGV[1])
        return 1
    "#;
    let available_at = unix_time_millis()?.saturating_add(delay_secs.saturating_mul(1000));
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    redis::Script::new(RETRY_SCRIPT)
        .key(NEW_POST_FANOUT_JOBS_ZSET)
        .arg(current_job)
        .arg(retry_job)
        .arg(available_at)
        .invoke_async::<i64>(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;
    Ok(())
}

// =============================================================================
// Notify Subscriptions ("bells")
// =============================================================================

/// Forward index: creators this subscriber has belled.
fn build_notify_subs_key(subscriber: &PublicKey) -> String {
    format!("notify_subs:{}", subscriber.to_hex())
}

/// `created_at:event_id` of the last applied notify-list event for this
/// subscriber. The id is carried so a `created_at` tie can resolve the way
/// NIP-01 resolves it, by lowest event id.
fn build_notify_subs_ts_key(subscriber: &PublicKey) -> String {
    format!("notify_subs_ts:{}", subscriber.to_hex())
}

/// Prefix of the reverse index. Kept as a prefix (not a built key) because the
/// Lua script below composes watcher keys from it.
const NOTIFY_WATCHERS_PREFIX: &str = "notify_watchers:";

/// Reverse index: subscribers watching this creator. The hot read path.
fn build_notify_watchers_key(creator: &PublicKey) -> String {
    format!("{}{}", NOTIFY_WATCHERS_PREFIX, creator.to_hex())
}

/// Rate-limit window marker for one (subscriber, creator) pair.
pub fn build_notify_rate_key(subscriber: &PublicKey, creator: &PublicKey) -> String {
    format!("notify_rate:{}:{}", subscriber.to_hex(), creator.to_hex())
}

/// Diff-and-apply a replacement notify list atomically.
///
/// `notify_subs` and `notify_watchers` are two views of the same relation and
/// must move together, so the whole diff runs in one Lua script rather than a
/// read-then-write from the caller.
///
/// The script re-checks the stored `created_at` internally: a relay can deliver
/// an older replacement after a newer one, and an advisory check in the caller
/// would still race. Returns `false` when the incoming event was rejected as
/// stale or duplicate, `true` when it was applied.
///
/// Ties on `created_at` resolve the way NIP-01 resolves them: "in case of
/// replaceable events with the same timestamp, the event with the lowest id
/// (first in lexical order) should be retained". Resolving by arrival order
/// instead would let this service and the relay hold permanently different
/// lists — a rebuild from relay history would then disagree with what we served
/// live. Note the direction: an incoming event with an *equal* timestamp wins
/// only when its id sorts *below* the stored one, which reads backwards from
/// "newer wins". An exact replay (same timestamp, same id) is allowed through
/// as an idempotent repair path for startup rebuilds.
///
/// `notify_subs_ts` therefore stores `created_at:event_id`. A bare integer left
/// by an earlier build is read as a timestamp with no known id, which can only
/// make the guard more conservative: ties against it are rejected, exactly as
/// they were before.
///
/// The atomicity is load-bearing because production runs more than one replica.
/// `try_claim_event` only prevents two replicas handling the *same* event; two
/// different list events from one subscriber can still land concurrently, and a
/// read-then-write would let the older one win. A single replica needs none of
/// this — its handler loop is sequential.
///
/// Atomic is not transactional, though: Redis runs the script without
/// interleaving anything else, but a script that dies partway through keeps the
/// writes it already made. So the write order holds one invariant at every
/// intermediate step — every `notify_watchers:{creator}` naming this subscriber
/// has `creator` in `notify_subs:{subscriber}`. Removals clear the reverse
/// index before the forward one and additions write the forward index first,
/// which leaves a half-applied script with `notify_subs` a *superset* of the
/// true relation. The next list reconciles that, because removals are computed
/// from it.
///
/// The opposite skew does not recover, which is why the forward set is diffed
/// rather than `DEL`d and rebuilt. `notify_subs` is the only record of which
/// `notify_watchers:*` keys hold this subscriber, so once it is short, the
/// missing creators are unreachable: `previous` comes back without them, their
/// `SREM` is never issued, and the subscriber keeps getting pushes for a
/// creator they unbelled. Republishing the list they actually hold does not
/// help — only re-belling that exact creator and unbelling again would, which
/// is not something a user would think to do.
///
/// `creators` must already be bounded by the caller
/// (`notify_list_max_creators`): the script runs as one blocking unit and Redis
/// is single-threaded, so an unbounded list stalls the instance for every user.
///
/// Not Redis Cluster safe: the script writes `notify_watchers:*` keys that are
/// not declared in `KEYS`, because the set of creators is only known from the
/// event body. This deployment uses single-instance Redis (see
/// `docker-compose.yml`); moving to Cluster requires resharding this into one
/// call per creator slot or a hash-tagged key layout.
pub async fn replace_notify_subscriptions(
    pool: &RedisPool,
    subscriber: &PublicKey,
    creators: &[PublicKey],
    created_at: u64,
    event_id: &EventId,
) -> Result<bool> {
    const REPLACE_SCRIPT: &str = r#"
        local incoming_at = tonumber(ARGV[1])
        local incoming_id = ARGV[4]

        local stored = redis.call('GET', KEYS[2])
        if stored then
          -- `created_at:event_id`, or a bare integer written by an earlier
          -- build. An unparseable value is treated as absent rather than
          -- wedging the subscriber's list forever.
          local stored_at, stored_id = string.match(stored, '^(%d+):(%x+)$')
          if stored_at then
            stored_at = tonumber(stored_at)
          else
            stored_at = tonumber(stored)
            stored_id = nil
          end

          if stored_at then
            if stored_at > incoming_at then
              return 0
            end
            if stored_at == incoming_at then
              -- NIP-01 retains the lowest id on a tie, so the incoming event
              -- has to sort below the stored one, or be the exact same event
              -- replayed during a rebuild. With no stored id there is nothing
              -- to compare, so the tie stays rejected.
              if not stored_id or incoming_id > stored_id then
                return 0
              end
            end
          end
        end

        local prefix = ARGV[2]
        local subscriber = ARGV[3]

        local incoming = {}
        for i = 5, #ARGV do
          incoming[ARGV[i]] = true
        end

        -- Drop the subscriber from creators no longer on the list, reverse
        -- index first. Applying the difference rather than replacing the
        -- forward set wholesale is what keeps a half-applied script
        -- recoverable; see the ordering note in the function doc. An empty
        -- incoming list is legitimate (the user unbelled everyone) and Redis
        -- drops the key once its last member is removed.
        local previous = redis.call('SMEMBERS', KEYS[1])
        for _, creator in ipairs(previous) do
          if not incoming[creator] then
            redis.call('SREM', prefix .. creator, subscriber)
            redis.call('SREM', KEYS[1], creator)
          end
        end

        -- Add the new ones, forward index first, for the same reason. Both
        -- writes are unconditional, and the second one is why: re-asserting the
        -- reverse entry for a creator already on the forward set is exactly
        -- what lets an exact replay repair a `notify_watchers:*` that Redis
        -- lost while `notify_subs:*` survived. Skipping either write when the
        -- creator "is already applied" would take that repair away, and the
        -- forward `SADD` it would save is a no-op on a member anyway.
        for i = 5, #ARGV do
          redis.call('SADD', KEYS[1], ARGV[i])
          redis.call('SADD', prefix .. ARGV[i], subscriber)
        end

        -- ARGV[1] verbatim rather than `incoming_at`, so the stored timestamp
        -- is the caller's decimal string and never Lua's float formatting.
        redis.call('SET', KEYS[2], ARGV[1] .. ':' .. incoming_id)
        return 1
    "#;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let script = redis::Script::new(REPLACE_SCRIPT);
    let mut invocation = script.prepare_invoke();
    invocation
        .key(build_notify_subs_key(subscriber))
        .key(build_notify_subs_ts_key(subscriber))
        .arg(created_at)
        .arg(NOTIFY_WATCHERS_PREFIX)
        .arg(subscriber.to_hex())
        .arg(event_id.to_hex());
    for creator in creators {
        invocation.arg(creator.to_hex());
    }

    let applied: i64 = invocation
        .invoke_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(applied == 1)
}

/// Read every subscriber watching `creator` in one `SMEMBERS`.
///
/// **Not the fan-out read.** Delivery pages with `get_notify_watchers_page`,
/// and the bell fallback asks `is_notify_watcher`; this is the unbounded read
/// both of those exist to keep off the hot path, and it has no caller in `src/`.
/// It survives as the whole-set assertion helper the notify-list tests are
/// written against. Do not reintroduce it into delivery.
///
/// Unparseable members are skipped with a warning rather than failing the whole
/// lookup, so one corrupt entry cannot block delivery to everyone else.
pub async fn get_notify_watchers(pool: &RedisPool, creator: &PublicKey) -> Result<Vec<PublicKey>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let members: Vec<String> = redis::cmd("SMEMBERS")
        .arg(build_notify_watchers_key(creator))
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    let mut watchers = Vec::with_capacity(members.len());
    for member in members {
        match PublicKey::from_hex(&member) {
            Ok(pubkey) => watchers.push(pubkey),
            Err(e) => tracing::warn!(
                creator = %creator.to_hex(),
                member = %member,
                error = %e,
                "Skipping unparseable notify watcher"
            ),
        }
    }

    Ok(watchers)
}

/// One page of subscribers watching a creator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyWatcherPage {
    pub watchers: Vec<PublicKey>,
    pub next_cursor: u64,
}

/// Read one SSCAN page of subscribers watching `creator`.
///
/// The video fan-out path loops over pages so each Redis read and delivery
/// batch stays bounded without dropping subscribers beyond a permanent cap.
/// Unparseable members are skipped with a warning rather than failing the page.
pub async fn get_notify_watchers_page(
    pool: &RedisPool,
    creator: &PublicKey,
    cursor: u64,
    count: usize,
) -> Result<NotifyWatcherPage> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let (next_cursor, members): (u64, Vec<String>) = redis::cmd("SSCAN")
        .arg(build_notify_watchers_key(creator))
        .arg(cursor)
        .arg("COUNT")
        .arg(count)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    let mut seen = HashSet::with_capacity(members.len());
    let mut watchers = Vec::with_capacity(members.len());
    for member in members {
        match PublicKey::from_hex(&member) {
            Ok(pubkey) => {
                if seen.insert(pubkey) {
                    watchers.push(pubkey);
                }
            }
            Err(e) => tracing::warn!(
                creator = %creator.to_hex(),
                member = %member,
                error = %e,
                "Skipping unparseable notify watcher"
            ),
        }
    }

    Ok(NotifyWatcherPage {
        watchers,
        next_cursor,
    })
}

/// Test whether `subscriber` watches `creator`.
///
/// The membership question on its own. The bell fallback in
/// `send_notification_to_user` asks it per muted-mention recipient, and it is
/// the whole answer it needs, so it must not pay `get_notify_watchers`'s
/// transfer-and-parse of a creator's entire watcher set. `SISMEMBER` answers
/// in the server. Fan-out reads use `get_notify_watchers_page`.
pub async fn is_notify_watcher(
    pool: &RedisPool,
    creator: &PublicKey,
    subscriber: &PublicKey,
) -> Result<bool> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    redis::cmd("SISMEMBER")
        .arg(build_notify_watchers_key(creator))
        .arg(subscriber.to_hex())
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)
}

// =============================================================================
// Caching
// =============================================================================

/// Get a cached string value from Redis
pub async fn get_cached_string(pool: &RedisPool, key: &str) -> Result<Option<String>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let value: Option<String> = redis::cmd("GET")
        .arg(key)
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(value)
}

/// Set a cached string value in Redis with TTL
pub async fn set_cached_string(
    pool: &RedisPool,
    key: &str,
    value: &str,
    ttl_secs: u64,
) -> Result<()> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    redis::cmd("SETEX")
        .arg(key)
        .arg(ttl_secs)
        .arg(value)
        .query_async::<()>(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_user_tokens_key() {
        let pubkey =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let key = build_user_tokens_key(&pubkey);
        assert!(key.starts_with("user_tokens:"));
        assert!(key.contains(&pubkey.to_hex()));
    }

    #[tokio::test]
    async fn durable_fanout_job_is_reclaimed_and_advances_atomically() {
        let base_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
        let Ok(mut redis_url) = url::Url::parse(&base_url) else {
            return;
        };
        // Keep the global production queue out of this lease-expiry test.
        redis_url.set_path("/14");
        let Ok(pool) = create_pool(redis_url.as_str(), 2).await else {
            return;
        };
        let Ok(mut conn) = pool.get().await else {
            return;
        };
        if redis::cmd("PING")
            .query_async::<String>(&mut *conn)
            .await
            .is_err()
        {
            return;
        }
        let event_id = EventId::all_zeros();
        redis::cmd("DEL")
            .arg(format!("fanout:enqueued:{}", event_id.to_hex()))
            .arg(NEW_POST_FANOUT_JOBS_ZSET)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        redis::cmd("SET")
            .arg(NEW_POST_FANOUT_JOBS_ZSET)
            .arg("wrong-type")
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        let first_job = r#"{"event":"first","cursor":0}"#;
        let next_job = r#"{"event":"first","cursor":9}"#;
        assert!(
            enqueue_initial_fanout_job(&pool, &event_id, first_job, 60)
                .await
                .is_err(),
            "a broken queue must reject the enqueue"
        );
        let marker = get_cached_string(&pool, &format!("fanout:enqueued:{}", event_id.to_hex()))
            .await
            .unwrap();
        assert!(
            marker.is_none(),
            "a failed queue write must not consume the event's enqueue marker"
        );
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(NEW_POST_FANOUT_JOBS_ZSET)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        assert!(enqueue_initial_fanout_job(&pool, &event_id, first_job, 60)
            .await
            .unwrap());
        assert!(
            !enqueue_initial_fanout_job(&pool, &event_id, first_job, 60)
                .await
                .unwrap(),
            "an event replay must not enqueue a second initial page"
        );

        assert_eq!(
            claim_fanout_job(&pool, 1).await.unwrap().as_deref(),
            Some(first_job)
        );
        assert!(
            claim_fanout_job(&pool, 1).await.unwrap().is_none(),
            "a live lease prevents concurrent processing"
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            claim_fanout_job(&pool, 30).await.unwrap().as_deref(),
            Some(first_job),
            "a crashed worker's page becomes available after its lease"
        );
        rescore_fanout_job(&pool, first_job, 0).await.unwrap();
        assert_eq!(
            claim_fanout_job(&pool, 30).await.unwrap().as_deref(),
            Some(first_job),
            "graceful shutdown can release a page without waiting for lease expiry"
        );

        let retry_job = r#"{"event":"first","cursor":0,"attempt":1}"#;
        retry_fanout_job(&pool, first_job, retry_job, 0)
            .await
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        let queued: usize = redis::cmd("ZCARD")
            .arg(NEW_POST_FANOUT_JOBS_ZSET)
            .query_async(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        assert_eq!(queued, 1, "retry must remove the previous leased member");
        assert_eq!(
            claim_fanout_job(&pool, 30).await.unwrap().as_deref(),
            Some(retry_job),
            "retry atomically replaces the leased member with its persisted attempt"
        );

        complete_fanout_job(&pool, retry_job, Some(next_job))
            .await
            .unwrap();
        assert_eq!(
            claim_fanout_job(&pool, 30).await.unwrap().as_deref(),
            Some(next_job),
            "completion removes the current page and exposes its successor"
        );
        complete_fanout_job(&pool, next_job, None).await.unwrap();
        assert!(claim_fanout_job(&pool, 30).await.unwrap().is_none());

        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("fanout:enqueued:{}", event_id.to_hex()))
            .arg(NEW_POST_FANOUT_JOBS_ZSET)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }
}
