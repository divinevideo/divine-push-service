//! User notification preferences management for diVine
//!
//! Preferences are stored as a list of event kinds the user wants notifications for.
//! This is more flexible than boolean fields and aligns with Nostr's kind-centric model.

use serde::{Deserialize, Serialize};

use crate::config::DefaultPreferences;
use crate::error::{Result, ServiceError};
use crate::redis_store::RedisPool;

/// User notification preferences - list of kinds to receive notifications for
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserPreferences {
    /// Event kinds the user wants to receive notifications for
    pub kinds: Vec<u16>,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            kinds: vec![
                1,     // Text notes (comments, mentions)
                7,     // Reactions/likes
                16,    // Reposts
                30023, // Long-form content
                34236, // Videos from subscribed creators (new-post "bells")
            ],
        }
    }
}

impl From<&DefaultPreferences> for UserPreferences {
    fn from(defaults: &DefaultPreferences) -> Self {
        Self {
            kinds: defaults.kinds.clone(),
        }
    }
}

impl UserPreferences {
    /// Check if a specific kind is enabled
    pub fn is_kind_enabled(&self, kind: u16) -> bool {
        self.kinds.contains(&kind)
    }
}

/// Notification type enum for display purposes and kind mapping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationType {
    Like,
    Comment,
    Mention,
    Repost,
    /// A NIP-17 gift wrap addressed to the recipient.
    ///
    /// The wrapper reveals neither the real sender nor the message, so its push
    /// copy must remain generic.
    // TODO(#65): Route this only after the service receives classified gift wraps.
    DirectMessage,
    /// A creator the recipient subscribed to published a new video.
    ///
    /// Unlike every other variant this is not triggered by someone acting on the
    /// recipient's content, so its recipients come from the notify-list reverse
    /// index rather than the trigger event's `p` tags.
    NewPost,
}

impl NotificationType {
    /// Get the event kind this notification type corresponds to
    pub fn kind(&self) -> u16 {
        match self {
            NotificationType::Like => 7,
            NotificationType::Comment => 1,
            NotificationType::Mention => 1, // Same as Comment - both are kind 1
            NotificationType::Repost => 16,
            NotificationType::DirectMessage => 1059,
            // Distinct from Mention's kind 1 even though video mentions also
            // arrive on kind 34236 events, so the two toggle independently.
            NotificationType::NewPost => 34236,
        }
    }

    /// Check if this notification type is enabled for the user
    pub fn is_enabled(&self, prefs: &UserPreferences) -> bool {
        prefs.is_kind_enabled(self.kind())
    }

    /// Get a display name for the notification type
    pub fn display_name(&self) -> &'static str {
        match self {
            NotificationType::Like => "like",
            NotificationType::Comment => "comment",
            NotificationType::Mention => "mention",
            NotificationType::Repost => "repost",
            NotificationType::DirectMessage => "directMessage",
            NotificationType::NewPost => "newPost",
        }
    }
}

/// Redis key prefix for user preferences
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

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let key = build_preferences_key(pubkey);

    // Get JSON string from Redis
    let result: Option<String> = conn.get(&key).await.map_err(ServiceError::Redis)?;

    match result {
        Some(json_str) => {
            // Parse JSON into UserPreferences
            match serde_json::from_str(&json_str) {
                Ok(prefs) => Ok(prefs),
                Err(e) => {
                    tracing::warn!(
                        pubkey = pubkey,
                        error = %e,
                        "Failed to parse stored preferences, returning defaults"
                    );
                    Ok(UserPreferences::from(defaults))
                }
            }
        }
        None => Ok(UserPreferences::from(defaults)),
    }
}

/// Set user preferences in Redis
pub async fn set_user_preferences(
    pool: &RedisPool,
    pubkey: &str,
    prefs: &UserPreferences,
) -> Result<()> {
    use redis::AsyncCommands;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let key = build_preferences_key(pubkey);

    // Serialize to JSON
    let json_str = serde_json::to_string(prefs)
        .map_err(|e| ServiceError::Internal(format!("Failed to serialize preferences: {}", e)))?;

    // Store in Redis (no expiration - preferences persist until deleted)
    let _: () = conn
        .set(&key, &json_str)
        .await
        .map_err(ServiceError::Redis)?;

    Ok(())
}

/// Delete user preferences from Redis (called on deregistration)
pub async fn delete_user_preferences(pool: &RedisPool, pubkey: &str) -> Result<()> {
    use redis::AsyncCommands;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| ServiceError::Internal(format!("Failed to get Redis connection: {}", e)))?;
    let key = build_preferences_key(pubkey);

    let _: () = conn.del(&key).await.map_err(ServiceError::Redis)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_preferences() {
        let prefs = UserPreferences::default();
        assert!(prefs.kinds.contains(&1));
        assert!(!prefs.kinds.contains(&3));
        assert!(prefs.kinds.contains(&7));
        assert!(prefs.kinds.contains(&16));
        assert!(prefs.kinds.contains(&30023));
    }

    #[test]
    fn test_notification_type_kind_mapping() {
        assert_eq!(NotificationType::Like.kind(), 7);
        assert_eq!(NotificationType::Comment.kind(), 1);
        assert_eq!(NotificationType::Mention.kind(), 1);
        assert_eq!(NotificationType::Repost.kind(), 16);
        assert_eq!(NotificationType::DirectMessage.kind(), 1059);
    }

    #[test]
    fn test_notification_type_enabled() {
        // All kinds enabled
        let prefs = UserPreferences::default();
        assert!(NotificationType::Like.is_enabled(&prefs));
        assert!(NotificationType::Comment.is_enabled(&prefs));
        assert!(NotificationType::Mention.is_enabled(&prefs));

        // Only kind 7 enabled
        let prefs = UserPreferences { kinds: vec![7] };
        assert!(NotificationType::Like.is_enabled(&prefs));
        assert!(!NotificationType::Comment.is_enabled(&prefs));
        assert!(!NotificationType::Mention.is_enabled(&prefs));
        assert!(!NotificationType::DirectMessage.is_enabled(&prefs));
    }

    #[test]
    fn test_comment_and_mention_share_kind() {
        // Both Comment and Mention are kind 1, so enabling/disabling kind 1 affects both
        let prefs_with_1 = UserPreferences { kinds: vec![1, 7] };
        assert!(NotificationType::Comment.is_enabled(&prefs_with_1));
        assert!(NotificationType::Mention.is_enabled(&prefs_with_1));

        let prefs_without_1 = UserPreferences { kinds: vec![7, 16] };
        assert!(!NotificationType::Comment.is_enabled(&prefs_without_1));
        assert!(!NotificationType::Mention.is_enabled(&prefs_without_1));
    }

    #[test]
    fn test_new_post_kind_is_video_kind() {
        assert_eq!(NotificationType::NewPost.kind(), 34236);
    }

    #[test]
    fn test_direct_message_preference_uses_gift_wrap_kind() {
        let enabled = UserPreferences { kinds: vec![1059] };
        let disabled = UserPreferences {
            kinds: vec![1, 7, 16],
        };

        assert!(NotificationType::DirectMessage.is_enabled(&enabled));
        assert!(!NotificationType::DirectMessage.is_enabled(&disabled));
        assert_eq!(
            NotificationType::DirectMessage.display_name(),
            "directMessage"
        );
    }

    #[test]
    fn test_new_post_display_name_is_wire_contract() {
        // divine-mobile matches this exact string in notificationKindFromPushType.
        assert_eq!(NotificationType::NewPost.display_name(), "newPost");
    }

    #[test]
    fn test_new_post_disabled_when_prefs_omit_video_kind() {
        let prefs = UserPreferences {
            kinds: vec![1, 7, 16, 30023],
        };
        assert!(!NotificationType::NewPost.is_enabled(&prefs));
    }

    #[test]
    fn test_new_post_enabled_under_shipped_defaults() {
        let prefs = UserPreferences::default();
        assert!(NotificationType::NewPost.is_enabled(&prefs));
    }

    #[test]
    fn test_new_post_and_mention_toggle_independently() {
        // Mention is kind 1, NewPost is 34236, so a user can mute new-post
        // pushes without muting video mentions.
        let only_mentions = UserPreferences { kinds: vec![1] };
        assert!(NotificationType::Mention.is_enabled(&only_mentions));
        assert!(!NotificationType::NewPost.is_enabled(&only_mentions));

        let only_new_posts = UserPreferences { kinds: vec![34236] };
        assert!(!NotificationType::Mention.is_enabled(&only_new_posts));
        assert!(NotificationType::NewPost.is_enabled(&only_new_posts));
    }

    #[test]
    fn test_build_preferences_key() {
        let key = build_preferences_key("abc123");
        assert_eq!(key, "user_preferences:abc123");
    }

    #[test]
    fn test_is_kind_enabled() {
        let prefs = UserPreferences {
            kinds: vec![1, 7, 30023],
        };
        assert!(prefs.is_kind_enabled(1));
        assert!(prefs.is_kind_enabled(7));
        assert!(prefs.is_kind_enabled(30023));
        assert!(!prefs.is_kind_enabled(3));
        assert!(!prefs.is_kind_enabled(16));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let prefs = UserPreferences {
            kinds: vec![1, 7, 16],
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let parsed: UserPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(prefs, parsed);
    }
}
