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
use std::time::Duration;

// Type alias for the connection pool
pub type RedisPool = Pool<RedisConnectionManager>;

// Redis key constants
const PROCESSED_EVENTS_SET: &str = "processed_nostr_events";
const STALE_TOKENS_ZSET: &str = "stale_tokens";
const TOKEN_TO_PUBKEY_HASH: &str = "token_to_pubkey";

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

/// Checks if an event has already been processed.
pub async fn is_event_processed(pool: &RedisPool, event_id: &EventId) -> Result<bool> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let processed: bool = redis::cmd("SISMEMBER")
        .arg(PROCESSED_EVENTS_SET)
        .arg(event_id.to_hex())
        .query_async(&mut *conn)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(processed)
}

/// Marks an event as processed with a time-to-live (TTL).
pub async fn mark_event_processed(
    pool: &RedisPool,
    event_id: &EventId,
    ttl_seconds: u64,
) -> Result<()> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;

    let ttl_i64: i64 = ttl_seconds
        .try_into()
        .map_err(|_| ServiceError::Internal("TTL value too large".to_string()))?;

    let mut pipe = redis::pipe();
    pipe.atomic()
        .sadd(PROCESSED_EVENTS_SET, event_id.to_hex())
        .expire(PROCESSED_EVENTS_SET, ttl_i64);

    pipe.query_async::<Value>(&mut *conn)
        .await
        .map(|_| ())
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
}
