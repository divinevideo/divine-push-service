//! Nostr relay listener for diVine Push Service
//!
//! Subscribes to fixed event kinds and forwards events to the handler.
//! No dynamic subscription management - uses predefined notification kinds.

use crate::{
    error::{Result, ServiceError},
    event_handler::EventContext,
    state::AppState,
};
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Control event kinds for push notification management
const KIND_REGISTRATION: u16 = 3079;
const KIND_DEREGISTRATION: u16 = 3080;
const KIND_PREFERENCES_UPDATE: u16 = 3083;

/// Content event kinds that trigger notifications
const KIND_TEXT_NOTE: u16 = 1;
const KIND_CONTACT_LIST: u16 = 3;
const KIND_REACTION: u16 = 7;
const KIND_REPOST: u16 = 16;
const KIND_LONG_FORM: u16 = 30023;

pub struct NostrListener {
    state: Arc<AppState>,
}

impl NostrListener {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(
        &self,
        event_tx: Sender<(Box<Event>, EventContext)>,
        token: CancellationToken,
    ) -> Result<()> {
        info!("Starting diVine Nostr listener...");

        let service_keys = self.state.service_keys.clone().ok_or_else(|| {
            ServiceError::Internal("Nostr service keys not configured".to_string())
        })?;
        let service_pubkey = service_keys.public_key();

        // Ensure the client is connected
        if !self.is_connected().await {
            self.ensure_connected().await?;
        }

        // Process historical control events first
        self.process_historical_events(&event_tx, &service_pubkey, &token)
            .await?;

        // Subscribe to live events
        self.subscribe_to_live_events(&token).await?;

        // Main event loop
        self.process_live_events(event_tx, service_pubkey, token)
            .await?;

        info!("Nostr listener shutting down.");
        Ok(())
    }

    async fn is_connected(&self) -> bool {
        let relays = self.state.nostr_client.relays().await;
        !relays.is_empty() && relays.values().any(|s| s.is_connected())
    }

    async fn ensure_connected(&self) -> Result<()> {
        warn!("Nostr client not connected. Attempting to connect...");
        let relay_url = &self.state.settings.nostr.relay_url;

        if relay_url.is_empty() {
            return Err(ServiceError::Internal(
                "Nostr relay URL missing in settings".to_string(),
            ));
        }

        self.state
            .nostr_client
            .add_relay(relay_url.as_str())
            .await?;
        self.state.nostr_client.connect().await;

        if !self.is_connected().await {
            return Err(ServiceError::Internal(
                "Failed to connect to Nostr relay".to_string(),
            ));
        }

        info!("Successfully connected to Nostr relay");
        Ok(())
    }

    async fn process_historical_events(
        &self,
        event_tx: &Sender<(Box<Event>, EventContext)>,
        service_pubkey: &PublicKey,
        token: &CancellationToken,
    ) -> Result<()> {
        let process_window_duration = Duration::from_secs(
            self.state.settings.service.process_window_days as u64 * 24 * 60 * 60,
        );
        let since_timestamp = Timestamp::now() - process_window_duration;

        // Build filter for control kinds only (registration/deregistration/preferences)
        let control_kinds = vec![
            Kind::from(KIND_REGISTRATION),
            Kind::from(KIND_DEREGISTRATION),
            Kind::from(KIND_PREFERENCES_UPDATE),
        ];

        let filter = Filter::new().kinds(control_kinds).since(since_timestamp);

        info!(since = %since_timestamp, "Querying historical control events...");

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cancelled before historical event query");
                return Ok(());
            }
            fetch_result = self.state.nostr_client.fetch_events(filter, Duration::from_secs(60)) => {
                match fetch_result {
                    Ok(historical_events) => {
                        info!(count = historical_events.len(), "Processing historical control events...");

                        for event in historical_events {
                            if event.pubkey == *service_pubkey {
                                continue;
                            }

                            tokio::select! {
                                biased;
                                _ = token.cancelled() => {
                                    info!("Cancelled during historical processing");
                                    return Ok(());
                                }
                                send_res = event_tx.send((Box::new(event), EventContext::Historical)) => {
                                    if let Err(e) = send_res {
                                        error!("Failed to send historical event: {}", e);
                                        return Err(ServiceError::Internal(
                                            "Event handler channel closed".to_string()
                                        ));
                                    }
                                }
                            }
                        }

                        info!("Finished processing historical control events");
                    }
                    Err(e) => {
                        error!("Failed to query historical control events: {}", e);
                        warn!("Proceeding without historical events");
                    }
                }
            }
        }

        Ok(())
    }

    async fn subscribe_to_live_events(&self, token: &CancellationToken) -> Result<()> {
        // Subscribe to all relevant kinds for diVine
        let all_kinds = vec![
            // Control kinds
            Kind::from(KIND_REGISTRATION),
            Kind::from(KIND_DEREGISTRATION),
            Kind::from(KIND_PREFERENCES_UPDATE),
            // Notification trigger kinds
            Kind::from(KIND_TEXT_NOTE),
            Kind::from(KIND_CONTACT_LIST),
            Kind::from(KIND_REACTION),
            Kind::from(KIND_REPOST),
            Kind::from(KIND_LONG_FORM),
        ];

        // Look back 1 hour to catch any recent events
        let since = Timestamp::now() - Duration::from_secs(60 * 60);

        let filter = Filter::new().kinds(all_kinds.clone()).since(since);

        info!(
            "Subscribing to event kinds: {:?}",
            all_kinds.iter().map(|k| k.as_u16()).collect::<Vec<_>>()
        );

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cancelled before live subscription");
                return Ok(());
            }
            sub_result = self.state.nostr_client.subscribe(filter, None) => {
                match sub_result {
                    Ok(_output) => {
                        info!("Successfully subscribed to diVine notification kinds");
                    }
                    Err(e) => {
                        error!("Failed to subscribe to notification kinds: {}", e);
                        return Err(e.into());
                    }
                }
            }
        }

        Ok(())
    }

    async fn process_live_events(
        &self,
        event_tx: Sender<(Box<Event>, EventContext)>,
        service_pubkey: PublicKey,
        token: CancellationToken,
    ) -> Result<()> {
        let mut notifications = self.state.nostr_client.notifications();

        info!("Processing live events...");

        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    info!("Cancellation received, shutting down");
                    break;
                }

                res = notifications.recv() => {
                    match res {
                        Ok(notification) => {
                            match notification {
                                RelayPoolNotification::Event { event, .. } => {
                                    if event.pubkey == service_pubkey {
                                        debug!("Skipping event from service account");
                                        continue;
                                    }

                                    let event_id = event.id;
                                    let event_kind = event.kind;

                                    debug!(event_id = %event_id, kind = %event_kind, "Received live event");

                                    tokio::select! {
                                        biased;
                                        _ = token.cancelled() => {
                                            info!("Cancelled while sending event");
                                            break;
                                        }
                                        send_res = event_tx.send((event, EventContext::Live)) => {
                                            if let Err(e) = send_res {
                                                error!("Failed to send live event: {}", e);
                                                break;
                                            }
                                        }
                                    }
                                }
                                RelayPoolNotification::Message { relay_url, message } => {
                                    debug!(%relay_url, ?message, "Received relay message");
                                }
                                RelayPoolNotification::Shutdown => {
                                    info!("Received shutdown notification");
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            error!("Error receiving notification: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
