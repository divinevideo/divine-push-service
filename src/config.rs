use serde::Deserialize;

// Re-export config crate error if needed, or use custom error
pub use config::ConfigError;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub nostr: NostrSettings,
    pub service: ServiceSettings,
    pub redis: RedisSettings,
    pub app: AppConfig,  // Single app instead of Vec<AppConfig>
    pub cleanup: CleanupSettings,
    #[serde(default = "default_server_settings")]
    pub server: ServerSettings,
    #[serde(default)]
    pub notification: NotificationSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NostrSettings {
    pub relay_url: String,
    #[serde(default)]
    pub profile_relays: Vec<String>,
    #[serde(default = "default_profile_cache_ttl")]
    pub profile_cache_ttl_secs: u64,
    #[serde(default = "default_query_timeout")]
    pub query_timeout_secs: u64,
}

fn default_profile_cache_ttl() -> u64 {
    300 // 5 minutes
}

fn default_query_timeout() -> u64 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceSettings {
    pub private_key_hex: Option<String>,
    #[serde(default = "default_process_window_days")]
    pub process_window_days: i64,
    #[serde(default = "default_processed_event_ttl")]
    pub processed_event_ttl_secs: u64,
}

fn default_process_window_days() -> i64 {
    7
}

fn default_processed_event_ttl() -> u64 {
    604800 // 7 days
}

#[derive(Debug, Deserialize, Clone)]
pub struct RedisSettings {
    pub url: String,
    #[serde(default = "default_pool_size")]
    pub connection_pool_size: u32,
}

fn default_pool_size() -> u32 {
    10
}

#[derive(Debug, Deserialize, Clone)]
pub struct FirebaseConfig {
    pub project_id: String,
    /// Path to Firebase service account JSON. If empty/omitted, uses Application Default Credentials
    /// (e.g. GKE Workload Identity).
    #[serde(default)]
    pub credentials_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub name: String,
    pub package: String,  // App package name (e.g., co.openvine.app)
    pub firebase: FirebaseConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CleanupSettings {
    #[serde(default = "default_cleanup_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cleanup_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_token_max_age")]
    pub token_max_age_days: i64,
}

fn default_cleanup_enabled() -> bool {
    true
}

fn default_cleanup_interval() -> u64 {
    86400 // 1 day
}

fn default_token_max_age() -> i64 {
    90
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerSettings {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
}

fn default_server_settings() -> ServerSettings {
    ServerSettings {
        listen_addr: default_listen_addr(),
    }
}

fn default_listen_addr() -> String {
    "0.0.0.0:8000".to_string()
}

/// Notification configuration for diVine
#[derive(Debug, Deserialize, Clone, Default)]
pub struct NotificationSettings {
    /// Event kinds to subscribe to for notifications
    #[serde(default = "default_event_kinds")]
    pub event_kinds: Vec<u64>,
    /// Default notification preferences for new users
    #[serde(default)]
    pub default_preferences: DefaultPreferences,
}

fn default_event_kinds() -> Vec<u64> {
    vec![
        1,     // Text notes (comments, mentions)
        3,     // Contact list (follows)
        7,     // Reactions/likes (NIP-25)
        16,    // Generic reposts (NIP-18)
        30023, // Long-form content (mentions)
    ]
}

/// Default notification preferences - kinds the user wants notifications for
#[derive(Debug, Deserialize, Clone)]
pub struct DefaultPreferences {
    /// Event kinds enabled by default for new users
    #[serde(default = "default_preference_kinds")]
    pub kinds: Vec<u16>,
}

fn default_preference_kinds() -> Vec<u16> {
    vec![
        1,     // Text notes (comments, mentions)
        3,     // Contact list (follows)
        7,     // Reactions/likes
        16,    // Reposts
        30023, // Long-form content
    ]
}

impl Default for DefaultPreferences {
    fn default() -> Self {
        Self {
            kinds: default_preference_kinds(),
        }
    }
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let config_dir = std::env::current_dir().expect("Failed to get current dir");

        // Default to development environment for safety
        let env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".to_string());

        // Select config file based on environment
        let config_filename = match env.as_str() {
            "production" => "settings.yaml",
            "development" => "settings.development.yaml",
            other => {
                // For any other environment, try settings.{env}.yaml
                &format!("settings.{}.yaml", other)
            }
        };

        let config_path = config_dir.join("config").join(config_filename);

        let s = config::Config::builder()
            .add_source(config::File::from(config_path).required(true))
            // Eg.. `NOSTR_PUSH__REDIS__URL=redis://...` would override `redis.url`
            .add_source(config::Environment::with_prefix("NOSTR_PUSH").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    /// Get service keys for relay auth and NIP-44 encryption
    pub fn get_service_keys(&self) -> Option<nostr_sdk::Keys> {
        let key_hex = self.service.private_key_hex.as_deref()?;
        let secret_key = nostr_sdk::SecretKey::from_hex(key_hex).ok()?;
        Some(nostr_sdk::Keys::new(secret_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_preferences() {
        let prefs = DefaultPreferences::default();
        assert!(prefs.kinds.contains(&1));   // Text notes
        assert!(prefs.kinds.contains(&3));   // Follows
        assert!(prefs.kinds.contains(&7));   // Likes
        assert!(prefs.kinds.contains(&16));  // Reposts
        assert!(prefs.kinds.contains(&30023)); // Long-form
    }

    #[test]
    fn test_default_event_kinds() {
        let kinds = default_event_kinds();
        assert!(kinds.contains(&1));   // Text notes
        assert!(kinds.contains(&3));   // Contact list
        assert!(kinds.contains(&7));   // Reactions
        assert!(kinds.contains(&16));  // Reposts
        assert!(kinds.contains(&30023)); // Long-form
    }
}
