//! Integration tests for user preferences Redis operations

use divine_push_service::config::DefaultPreferences;
use divine_push_service::preferences::{
    self, build_preferences_key, NotificationType, UserPreferences,
};
use divine_push_service::redis_store;

/// Helper to get a Redis pool, skipping test if Redis unavailable
async fn get_test_pool() -> Option<redis_store::RedisPool> {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    match redis_store::create_pool(&redis_url, 5).await {
        Ok(pool) => Some(pool),
        Err(_) => {
            println!("Skipping test: Redis not available");
            None
        }
    }
}

fn test_pubkey() -> String {
    "test_prefs_pubkey_abc123".to_string()
}

fn default_prefs() -> DefaultPreferences {
    DefaultPreferences {
        kinds: vec![1, 3, 7, 16, 30023],
    }
}

#[tokio::test]
async fn test_preferences_roundtrip() {
    let Some(pool) = get_test_pool().await else {
        return;
    };

    let pubkey = test_pubkey();

    // Clean up first
    let _ = preferences::delete_user_preferences(&pool, &pubkey).await;

    // Get preferences before setting - should return defaults
    let prefs = preferences::get_user_preferences(&pool, &pubkey, &default_prefs())
        .await
        .expect("Should get default preferences");

    assert!(prefs.kinds.contains(&1));
    assert!(prefs.kinds.contains(&7));
    assert!(prefs.kinds.contains(&16));

    // Set custom preferences - only likes and reposts
    let custom_prefs = UserPreferences { kinds: vec![7, 16] };

    preferences::set_user_preferences(&pool, &pubkey, &custom_prefs)
        .await
        .expect("Should set preferences");

    // Get preferences back
    let retrieved = preferences::get_user_preferences(&pool, &pubkey, &default_prefs())
        .await
        .expect("Should get stored preferences");

    assert_eq!(retrieved.kinds.len(), 2);
    assert!(retrieved.kinds.contains(&7));
    assert!(retrieved.kinds.contains(&16));
    assert!(!retrieved.kinds.contains(&1));
    assert!(!retrieved.kinds.contains(&3));

    // Clean up
    let _ = preferences::delete_user_preferences(&pool, &pubkey).await;
}

#[tokio::test]
async fn test_preferences_delete() {
    let Some(pool) = get_test_pool().await else {
        return;
    };

    let pubkey = format!("{}_delete", test_pubkey());

    // Set preferences with no kinds (notifications disabled)
    let prefs = UserPreferences { kinds: vec![] };

    preferences::set_user_preferences(&pool, &pubkey, &prefs)
        .await
        .expect("Should set preferences");

    // Verify it was stored
    let retrieved = preferences::get_user_preferences(&pool, &pubkey, &default_prefs())
        .await
        .expect("Should get stored preferences");
    assert!(retrieved.kinds.is_empty());

    // Delete preferences
    preferences::delete_user_preferences(&pool, &pubkey)
        .await
        .expect("Should delete preferences");

    // Get should return defaults now
    let retrieved = preferences::get_user_preferences(&pool, &pubkey, &default_prefs())
        .await
        .expect("Should get default preferences after delete");

    // Should be back to defaults
    assert!(retrieved.kinds.contains(&1));
    assert!(retrieved.kinds.contains(&7));
}

#[tokio::test]
async fn test_notification_type_filtering_with_kinds() {
    // Test that NotificationType.is_enabled correctly filters based on kinds

    // Only likes (kind 7) and reposts (kind 16) enabled
    let prefs = UserPreferences { kinds: vec![7, 16] };

    assert!(NotificationType::Like.is_enabled(&prefs)); // kind 7 ✓
    assert!(!NotificationType::Comment.is_enabled(&prefs)); // kind 1 ✗
    assert!(!NotificationType::Follow.is_enabled(&prefs)); // kind 3 ✗
    assert!(!NotificationType::Mention.is_enabled(&prefs)); // kind 1 ✗
    assert!(NotificationType::Repost.is_enabled(&prefs)); // kind 16 ✓
}

#[tokio::test]
async fn test_comment_and_mention_share_kind_1() {
    // Both Comment and Mention are kind 1
    // Enabling kind 1 enables both, disabling disables both

    let prefs_with_kind_1 = UserPreferences { kinds: vec![1, 7] };
    assert!(NotificationType::Comment.is_enabled(&prefs_with_kind_1));
    assert!(NotificationType::Mention.is_enabled(&prefs_with_kind_1));

    let prefs_without_kind_1 = UserPreferences { kinds: vec![7, 16] };
    assert!(!NotificationType::Comment.is_enabled(&prefs_without_kind_1));
    assert!(!NotificationType::Mention.is_enabled(&prefs_without_kind_1));
}

#[tokio::test]
async fn test_preferences_update() {
    let Some(pool) = get_test_pool().await else {
        return;
    };

    let pubkey = format!("{}_update", test_pubkey());

    // Clean up first
    let _ = preferences::delete_user_preferences(&pool, &pubkey).await;

    // Set initial preferences - all kinds enabled
    let initial = UserPreferences {
        kinds: vec![1, 3, 7, 16, 30023],
    };

    preferences::set_user_preferences(&pool, &pubkey, &initial)
        .await
        .expect("Should set initial preferences");

    // Update to fewer kinds
    let updated = UserPreferences { kinds: vec![7, 16] };

    preferences::set_user_preferences(&pool, &pubkey, &updated)
        .await
        .expect("Should update preferences");

    // Verify update
    let retrieved = preferences::get_user_preferences(&pool, &pubkey, &default_prefs())
        .await
        .expect("Should get updated preferences");

    assert_eq!(retrieved.kinds.len(), 2);
    assert!(retrieved.kinds.contains(&7));
    assert!(retrieved.kinds.contains(&16));
    assert!(!retrieved.kinds.contains(&1));

    // Clean up
    let _ = preferences::delete_user_preferences(&pool, &pubkey).await;
}

#[test]
fn test_build_preferences_key_format() {
    let key = build_preferences_key("abc123");
    assert_eq!(key, "user_preferences:abc123");

    let key2 = build_preferences_key("npub1xyz");
    assert_eq!(key2, "user_preferences:npub1xyz");
}

#[test]
fn test_notification_type_display_names() {
    assert_eq!(NotificationType::Like.display_name(), "like");
    assert_eq!(NotificationType::Comment.display_name(), "comment");
    assert_eq!(NotificationType::Follow.display_name(), "follow");
    assert_eq!(NotificationType::Mention.display_name(), "mention");
    assert_eq!(NotificationType::Repost.display_name(), "repost");
}

#[test]
fn test_notification_type_kind_values() {
    assert_eq!(NotificationType::Like.kind(), 7);
    assert_eq!(NotificationType::Comment.kind(), 1);
    assert_eq!(NotificationType::Follow.kind(), 3);
    assert_eq!(NotificationType::Mention.kind(), 1); // Same as Comment
    assert_eq!(NotificationType::Repost.kind(), 16);
}

#[test]
fn test_user_preferences_is_kind_enabled() {
    let prefs = UserPreferences {
        kinds: vec![1, 7, 30023],
    };

    assert!(prefs.is_kind_enabled(1));
    assert!(prefs.is_kind_enabled(7));
    assert!(prefs.is_kind_enabled(30023));
    assert!(!prefs.is_kind_enabled(3));
    assert!(!prefs.is_kind_enabled(16));
    assert!(!prefs.is_kind_enabled(9999));
}

#[test]
fn test_empty_preferences_disables_all() {
    let prefs = UserPreferences { kinds: vec![] };

    assert!(!NotificationType::Like.is_enabled(&prefs));
    assert!(!NotificationType::Comment.is_enabled(&prefs));
    assert!(!NotificationType::Follow.is_enabled(&prefs));
    assert!(!NotificationType::Mention.is_enabled(&prefs));
    assert!(!NotificationType::Repost.is_enabled(&prefs));
}

#[test]
fn test_json_serialization() {
    let prefs = UserPreferences {
        kinds: vec![1, 7, 16],
    };

    let json = serde_json::to_string(&prefs).unwrap();
    assert!(json.contains("\"kinds\""));
    assert!(json.contains("1"));
    assert!(json.contains("7"));
    assert!(json.contains("16"));

    let parsed: UserPreferences = serde_json::from_str(&json).unwrap();
    assert_eq!(prefs, parsed);
}
