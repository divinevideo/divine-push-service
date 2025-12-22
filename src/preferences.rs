//! User notification preferences management for diVine

use serde::{Deserialize, Serialize};

use crate::config::DefaultPreferences;
use crate::error::{Result, ServiceError};
use crate::redis_store::RedisPool;

/// User notification preferences
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserPreferences {
    pub likes: bool,
    pub comments: bool,
    pub follows: bool,
    pub mentions: bool,
    pub reposts: bool,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            likes: true,
            comments: true,
            follows: true,
            mentions: true,
            reposts: true,
        }
    }
}

impl From<&DefaultPreferences> for UserPreferences {
    fn from(defaults: &DefaultPreferences) -> Self {
        Self {
            likes: defaults.likes,
            comments: defaults.comments,
            follows: defaults.follows,
            mentions: defaults.mentions,
            reposts: defaults.reposts,
        }
    }
}

/// Notification type enum for matching events to preferences
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationType {
    Like,
    Comment,
    Follow,
    Mention,
    Repost,
}

impl NotificationType {
    /// Check if this notification type is enabled for the user
    pub fn is_enabled(&self, prefs: &UserPreferences) -> bool {
        match self {
            NotificationType::Like => prefs.likes,
            NotificationType::Comment => prefs.comments,
            NotificationType::Follow => prefs.follows,
            NotificationType::Mention => prefs.mentions,
            NotificationType::Repost => prefs.reposts,
        }
    }

    /// Get a display name for the notification type
    pub fn display_name(&self) -> &'static str {
        match self {
            NotificationType::Like => "like",
            NotificationType::Comment => "comment",
            NotificationType::Follow => "follow",
            NotificationType::Mention => "mention",
            NotificationType::Repost => "repost",
        }
    }
}

/// Redis keys for user preferences
const PREFERENCES_KEY_PREFIX: &str = "user_preferences:";

/// Build the Redis key for a user's preferences
pub fn build_preferences_key(pubkey: &str) -> String {
    format!("{}{}", PREFERENCES_KEY_PREFIX, pubkey)
}

/// Get user preferences from Redis, returning defaults if not set
pub async fn get_user_preferences(
    pool: &RedisPool,
    pubkey: &str,
    defaults: &DefaultPreferences,
) -> Result<UserPreferences> {
    use redis::AsyncCommands;

    let mut conn = pool.get().await.map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let key = build_preferences_key(pubkey);

    // Try to get all preference fields from the hash
    let result: redis::RedisResult<Vec<Option<String>>> = conn
        .hget(
            &key,
            &["likes", "comments", "follows", "mentions", "reposts"],
        )
        .await;

    match result {
        Ok(values) => {
            // If any values are present, use them; otherwise use defaults
            let has_any = values.iter().any(|v| v.is_some());
            if has_any {
                Ok(UserPreferences {
                    likes: values[0].as_ref().map(|v| v == "true").unwrap_or(defaults.likes),
                    comments: values[1].as_ref().map(|v| v == "true").unwrap_or(defaults.comments),
                    follows: values[2].as_ref().map(|v| v == "true").unwrap_or(defaults.follows),
                    mentions: values[3].as_ref().map(|v| v == "true").unwrap_or(defaults.mentions),
                    reposts: values[4].as_ref().map(|v| v == "true").unwrap_or(defaults.reposts),
                })
            } else {
                Ok(UserPreferences::from(defaults))
            }
        }
        Err(_) => Ok(UserPreferences::from(defaults)),
    }
}

/// Set user preferences in Redis
pub async fn set_user_preferences(
    pool: &RedisPool,
    pubkey: &str,
    prefs: &UserPreferences,
) -> Result<()> {
    use redis::AsyncCommands;

    let mut conn = pool.get().await.map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let key = build_preferences_key(pubkey);

    let _: () = conn
        .hset_multiple(
            &key,
            &[
                ("likes", prefs.likes.to_string()),
                ("comments", prefs.comments.to_string()),
                ("follows", prefs.follows.to_string()),
                ("mentions", prefs.mentions.to_string()),
                ("reposts", prefs.reposts.to_string()),
                ("updated_at", chrono::Utc::now().timestamp().to_string()),
            ],
        )
        .await
        .map_err(crate::error::ServiceError::Redis)?;

    Ok(())
}

/// Delete user preferences from Redis (called on deregistration)
pub async fn delete_user_preferences(
    pool: &RedisPool,
    pubkey: &str,
) -> Result<()> {
    use redis::AsyncCommands;

    let mut conn = pool.get().await.map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let key = build_preferences_key(pubkey);

    let _: () = conn.del(&key).await.map_err(crate::error::ServiceError::Redis)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_preferences() {
        let prefs = UserPreferences::default();
        assert!(prefs.likes);
        assert!(prefs.comments);
        assert!(prefs.follows);
        assert!(prefs.mentions);
        assert!(prefs.reposts);
    }

    #[test]
    fn test_notification_type_enabled() {
        let mut prefs = UserPreferences::default();
        assert!(NotificationType::Like.is_enabled(&prefs));

        prefs.likes = false;
        assert!(!NotificationType::Like.is_enabled(&prefs));
    }

    #[test]
    fn test_build_preferences_key() {
        let key = build_preferences_key("abc123");
        assert_eq!(key, "user_preferences:abc123");
    }
}
