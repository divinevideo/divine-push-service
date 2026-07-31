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

/// NIP-51 people list carrying new-post ("bell") subscriptions.
const KIND_NOTIFY_LIST: u16 = 30000;

/// Build the notify-list filter for the historical rebuild.
///
/// This must be a *separate* filter from the main kinds filter: an identifier
/// constraint there would wrongly apply to every kind in the list. Subscribing
/// to all of kind 30000 without the `#d` narrowing would pull every people list
/// on the relay.
///
/// Deliberately unbounded in time. A notify list is replaceable, so one
/// published months ago and never touched since is still current; a `since`
/// bound would silently drop those subscriptions. `limit` is the safety valve
/// on result size instead.
fn notify_list_history_filter(limit: usize) -> Filter {
    Filter::new()
        .kind(Kind::from(KIND_NOTIFY_LIST))
        .identifier(crate::event_handler::NOTIFY_LIST_D_TAG)
        .limit(limit)
}

/// Build the notify-list filter for the live subscription.
///
/// Unlike the historical query this is bounded in time and carries no `limit`.
/// Per NIP-01 a filter's `limit` applies to the initial stored-event query, so
/// reusing the history filter here makes every subscribe — including the
/// automatic resubscribe after a relay reconnect — replay up to `limit` stored
/// lists that the historical pass already applied. The `since` window mirrors
/// the main live filter and covers the gap between the historical fetch and the
/// subscription taking effect.
fn notify_list_live_filter(since: Timestamp) -> Filter {
    Filter::new()
        .kind(Kind::from(KIND_NOTIFY_LIST))
        .identifier(crate::event_handler::NOTIFY_LIST_D_TAG)
        .since(since)
}

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

        self.process_historical_notify_lists(event_tx, service_pubkey, token)
            .await
    }

    /// Replay notify lists from history.
    ///
    /// Mandatory, not an optimization: the reverse index lives only in Redis, so
    /// without this a restart against a fresh Redis silently drops every bell
    /// until each user happens to republish their list.
    async fn process_historical_notify_lists(
        &self,
        event_tx: &Sender<(Box<Event>, EventContext)>,
        service_pubkey: &PublicKey,
        token: &CancellationToken,
    ) -> Result<()> {
        let limit = self.state.settings.service.notify_list_history_limit;
        let filter = notify_list_history_filter(limit);

        info!(limit, "Querying historical notify lists...");

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cancelled before historical notify-list query");
                return Ok(());
            }
            fetch_result = self.state.nostr_client.fetch_events(filter, Duration::from_secs(60)) => {
                match fetch_result {
                    Ok(lists) => {
                        let count = lists.len();
                        if count >= limit {
                            warn!(
                                count,
                                limit,
                                "Historical notify-list query hit its limit; some subscriptions may be missing"
                            );
                        }
                        info!(count, "Processing historical notify lists...");

                        for event in lists {
                            if event.pubkey == *service_pubkey {
                                continue;
                            }

                            tokio::select! {
                                biased;
                                _ = token.cancelled() => {
                                    info!("Cancelled during historical notify-list processing");
                                    return Ok(());
                                }
                                send_res = event_tx.send((Box::new(event), EventContext::Historical)) => {
                                    if let Err(e) = send_res {
                                        error!("Failed to send historical notify list: {}", e);
                                        return Err(ServiceError::Internal(
                                            "Event handler channel closed".to_string()
                                        ));
                                    }
                                }
                            }
                        }

                        info!("Finished processing historical notify lists");
                    }
                    Err(e) => {
                        error!("Failed to query historical notify lists: {}", e);
                        warn!("Proceeding without historical notify lists - existing bells will not deliver until republished");
                    }
                }
            }
        }

        Ok(())
    }

    async fn subscribe_to_live_events(&self, token: &CancellationToken) -> Result<()> {
        let mut all_kinds = vec![
            // Control kinds
            Kind::from(KIND_REGISTRATION),
            Kind::from(KIND_DEREGISTRATION),
            Kind::from(KIND_PREFERENCES_UPDATE),
        ];

        for kind in &self.state.settings.notification.event_kinds {
            match u16::try_from(*kind) {
                Ok(kind) => {
                    let kind = Kind::from(kind);
                    if !all_kinds.contains(&kind) {
                        all_kinds.push(kind);
                    }
                }
                Err(_) => {
                    warn!(
                        kind,
                        "Skipping notification event kind outside Nostr u16 range"
                    );
                }
            }
        }

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

        // Notify lists need their own subscription: `subscribe` takes one filter
        // per call, and the `#d` narrowing cannot be merged into the kinds
        // filter above without wrongly constraining every other kind.
        let notify_filter = notify_list_live_filter(since);

        info!(
            "Subscribing to notify lists (kind {KIND_NOTIFY_LIST}, d={})",
            crate::event_handler::NOTIFY_LIST_D_TAG
        );

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cancelled before notify-list subscription");
                return Ok(());
            }
            sub_result = self.state.nostr_client.subscribe(notify_filter, None) => {
                match sub_result {
                    Ok(_output) => {
                        info!("Successfully subscribed to notify lists");
                    }
                    Err(e) => {
                        error!("Failed to subscribe to notify lists: {}", e);
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
