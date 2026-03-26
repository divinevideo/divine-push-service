//! Application state for diVine Push Service

use crate::{
    config::Settings,
    crypto::CryptoService,
    error::Result,
    fcm_sender::FcmClient,
    redis_store::{self, RedisPool},
    services::mention_parser::MentionParserService,
};
use nostr_sdk::{Client, ClientOptions, Keys};
use std::{env, sync::Arc};
use tracing::{info, warn};

use crate::error::ServiceError;

/// Shared application state for diVine Push Service
pub struct AppState {
    pub settings: Settings,
    pub redis_pool: RedisPool,
    pub fcm_client: Arc<FcmClient>,
    pub service_keys: Option<Keys>,
    pub crypto_service: Option<CryptoService>,
    pub nostr_client: Arc<Client>,
    pub profile_client: Arc<Client>,
    pub mention_parser_service: Option<Arc<MentionParserService>>,
}

impl AppState {
    pub async fn new(settings: Settings) -> Result<Self> {
        // Determine Redis URL: prioritize REDIS_URL env var over settings
        let redis_url = match env::var("REDIS_URL") {
            Ok(url_from_env) => {
                info!("Using Redis URL from REDIS_URL environment variable");
                url_from_env
            }
            Err(_) => {
                info!("REDIS_URL not set, using Redis URL from settings");
                settings.redis.url.clone()
            }
        };

        let redis_pool =
            redis_store::create_pool(&redis_url, settings.redis.connection_pool_size).await?;

        // Initialize FCM client for diVine
        let app_config = &settings.app;

        // Set credentials from config, or fall back to ADC (e.g. GKE Workload Identity)
        if let Some(ref creds_path) = app_config.firebase.credentials_path {
            env::set_var("GOOGLE_APPLICATION_CREDENTIALS", creds_path);
            info!(
                "Setting Firebase credentials for '{}': {}",
                app_config.name, creds_path
            );
        } else {
            info!(
                "No credentials_path configured for '{}', using Application Default Credentials",
                app_config.name
            );
        }

        let fcm_client = FcmClient::new(&app_config.firebase.project_id).map_err(|e| {
            ServiceError::Internal(format!(
                "Failed to initialize FCM client for {}: {}",
                app_config.name, e
            ))
        })?;

        info!(
            "Initialized FCM client for '{}' with project '{}'",
            app_config.name, app_config.firebase.project_id
        );

        let service_keys = settings.get_service_keys();

        // Create crypto service if we have service keys
        let crypto_service = service_keys
            .as_ref()
            .map(|keys| CryptoService::new(keys.clone()));

        // Create nostr client for event subscriptions
        let nostr_client = {
            let client = Client::default();
            if let Err(e) = client.add_relay(&settings.nostr.relay_url).await {
                warn!(
                    relay_url = %settings.nostr.relay_url,
                    error = %e,
                    "Failed to add main relay"
                );
            } else {
                info!(relay_url = %settings.nostr.relay_url, "Added main relay");
            }
            client.connect().await;
            Arc::new(client)
        };

        // Create dedicated profile client for metadata queries
        let profile_client = {
            let profile_opts = ClientOptions::default();

            let client = Client::builder().opts(profile_opts).build();

            // Add profile relays
            for relay_url in &settings.nostr.profile_relays {
                if let Err(e) = client.add_relay(relay_url).await {
                    warn!(relay_url = %relay_url, error = %e, "Failed to add profile relay");
                } else {
                    info!(relay_url = %relay_url, "Added profile relay");
                }
            }

            // Also add main relay as fallback
            let _ = client.add_relay(&settings.nostr.relay_url).await;

            client.connect().await;
            Arc::new(client)
        };

        // Initialize mention parser service
        let mention_parser_service = {
            // Wait a bit for connections to establish
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            Some(Arc::new(MentionParserService::new(
                redis_pool.clone(),
                profile_client.clone(),
                settings.nostr.profile_cache_ttl_secs,
            )))
        };

        info!("diVine Push Service state initialized successfully");

        Ok(AppState {
            settings,
            redis_pool,
            fcm_client: Arc::new(fcm_client),
            service_keys,
            crypto_service,
            nostr_client,
            profile_client,
            mention_parser_service,
        })
    }

    /// Get the service public key hex string
    pub fn service_pubkey_hex(&self) -> Option<String> {
        self.service_keys.as_ref().map(|k| k.public_key().to_hex())
    }
}
