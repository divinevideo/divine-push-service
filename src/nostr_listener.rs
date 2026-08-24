//! Nostr relay listener for diVine Push Service
//!
//! Subscribes to fixed event kinds and forwards events to the handler.
//! No dynamic subscription management - uses predefined notification kinds.

use crate::{
    error::{Result, ServiceError},
    event_handler::EventContext,
    state::AppState,
};
use async_trait::async_trait;
use nostr_sdk::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Control event kinds for push notification management
const KIND_REGISTRATION: u16 = 3079;
const KIND_DEREGISTRATION: u16 = 3080;
const KIND_PREFERENCES_UPDATE: u16 = 3083;

/// NIP-51 people list carrying new-post ("bell") subscriptions.
const KIND_NOTIFY_LIST: u16 = 30000;

const NOTIFICATION_SUBSCRIPTION_ID: &str = "divine-notifications";
const NOTIFY_LIST_SUBSCRIPTION_ID: &str = "divine-notify-lists";
/// Replays events that may have arrived while the subscription was stalled.
/// Successful event IDs remain claimed for seven days in the runtime config,
/// so this one-hour overlap recovers gaps without redelivering pushes.
const LIVE_EVENT_LOOKBACK: Duration = Duration::from_secs(60 * 60);
/// Give a relay time to clear a rate-limit window before replaying an hour of
/// traffic. An immediate retry is likely to receive the same rejection.
const RATE_LIMIT_RECOVERY_BACKOFF: Duration = Duration::from_secs(5);
/// Both owned subscriptions can be closed by one relay-side action. Keep the
/// first recovery open briefly so the queued closure for its sibling is not
/// mistaken for a rejection of the replacement subscription.
const CLOSED_RECOVERY_GRACE: Duration = Duration::from_secs(5);
/// Limit relay-driven recovery churn within one silence-watchdog window. A
/// relay that repeatedly closes subscriptions while still trickling events can
/// otherwise keep the listener healthy and replay the lookback indefinitely.
const MAX_CLOSED_RECOVERIES_PER_WINDOW: usize = 3;

/// How long to wait for the initial relay connection before treating startup as
/// failed. A listener exit now stops the process, so this must be long enough to
/// absorb a slow handshake and short enough that a genuinely unreachable relay
/// surfaces as a restart rather than a hang.
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the notify-list filter for the historical rebuild.
///
/// This must be a *separate* filter from the main kinds filter: an identifier
/// constraint there would wrongly apply to every kind in the list. Subscribing
/// to all of kind 30000 without the `#d` narrowing would pull every people list
/// on the relay.
///
/// Deliberately unbounded by `since`. A notify list is replaceable, so one
/// published months ago and never touched since is still current. Historical
/// rebuild pages backward with `until` instead of trusting one relay-limited
/// fetch to contain every current list.
fn notify_list_history_filter(limit: usize, until: Option<Timestamp>) -> Filter {
    let filter = Filter::new()
        .kind(Kind::from(KIND_NOTIFY_LIST))
        .identifier(crate::event_handler::NOTIFY_LIST_D_TAG)
        .limit(limit);
    match until {
        Some(until) => filter.until(until),
        None => filter,
    }
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

fn live_subscription_since() -> Timestamp {
    Timestamp::now() - LIVE_EVENT_LOOKBACK
}

fn is_owned_subscription(subscription_id: &SubscriptionId) -> bool {
    matches!(
        subscription_id.as_str(),
        NOTIFICATION_SUBSCRIPTION_ID | NOTIFY_LIST_SUBSCRIPTION_ID
    )
}

fn notification_event_kinds(configured_kinds: &[u64]) -> Vec<Kind> {
    let mut kinds = vec![
        Kind::from(KIND_REGISTRATION),
        Kind::from(KIND_DEREGISTRATION),
        Kind::from(KIND_PREFERENCES_UPDATE),
    ];

    for configured_kind in configured_kinds {
        match u16::try_from(*configured_kind) {
            Ok(kind) => {
                let kind = Kind::from(kind);
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            Err(_) => {
                warn!(
                    kind = configured_kind,
                    "Skipping notification event kind outside Nostr u16 range"
                );
            }
        }
    }

    kinds
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
        if !self
            .subscribe_to_live_events(&token, live_subscription_since())
            .await?
        {
            return Ok(());
        }

        // Main event loop
        self.process_live_events(event_tx, service_pubkey, token)
            .await?;

        info!("Nostr listener shutting down.");
        Ok(())
    }

    async fn is_connected(&self) -> bool {
        self.state
            .nostr_client
            .relay(self.state.settings.nostr.relay_url.as_str())
            .await
            .is_ok_and(|relay| relay.is_connected())
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

        // `connect` only spawns a background connection task per relay; it does
        // not wait for the handshake. Checking `is_connected` straight after
        // races the connection and reports failure on a perfectly healthy relay
        // — which, now that a listener exit takes the process down, would be a
        // startup crash loop. Wait for the connection before judging it.
        //
        // Deliberately not `try_connect`: that skips the background task, so a
        // relay that drops later would never reconnect. The production logs are
        // full of routine reconnects, so that task must exist.
        self.state.nostr_client.connect().await;
        self.state
            .nostr_client
            .wait_for_connection(RELAY_CONNECT_TIMEOUT)
            .await;

        if !self.is_connected().await {
            return Err(ServiceError::Internal(format!(
                "Failed to connect to Nostr relay within {:?}",
                RELAY_CONNECT_TIMEOUT
            )));
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
                            crate::metrics::event_received();

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
        let mut until = None;
        let mut seen = HashSet::new();
        let mut total = 0usize;

        info!(limit, "Querying historical notify lists...");

        loop {
            let filter = notify_list_history_filter(limit, until);

            let lists = tokio::select! {
                biased;
                _ = token.cancelled() => {
                    info!("Cancelled before historical notify-list query");
                    return Ok(());
                }
                fetch_result = self.state.nostr_client.fetch_events(filter, Duration::from_secs(60)) => {
                    match fetch_result {
                        Ok(lists) => lists,
                        Err(e) => {
                            error!("Failed to query historical notify lists: {}", e);
                            warn!("Proceeding with partial historical notify-list replay - existing bells may not deliver until republished");
                            break;
                        }
                    }
                }
            };

            let count = lists.len();
            if count == 0 {
                break;
            }
            info!(count, total, until = ?until, "Processing historical notify-list page...");

            let mut oldest = None;
            let mut new_count = 0usize;
            for event in lists {
                crate::metrics::event_received();

                oldest = match oldest {
                    Some(current) if current <= event.created_at => Some(current),
                    _ => Some(event.created_at),
                };

                if !seen.insert(event.id) {
                    continue;
                }
                new_count += 1;
                total += 1;

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

            if count < limit {
                break;
            }

            if new_count == 0 {
                warn!(
                    count,
                    limit,
                    until = ?until,
                    "Historical notify-list pagination made no progress; some subscriptions may be missing"
                );
                break;
            }

            until = oldest;
        }

        info!(count = total, "Finished processing historical notify lists");

        Ok(())
    }

    async fn subscribe_to_live_events(
        &self,
        token: &CancellationToken,
        since: Timestamp,
    ) -> Result<bool> {
        let all_kinds = notification_event_kinds(&self.state.settings.notification.event_kinds);
        let main_relay_url = RelayUrl::parse(&self.state.settings.nostr.relay_url)?;

        let filter = Filter::new().kinds(all_kinds.clone()).since(since);

        info!(
            "Subscribing to event kinds: {:?}",
            all_kinds.iter().map(|k| k.as_u16()).collect::<Vec<_>>()
        );

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cancelled before live subscription");
                return Ok(false);
            }
            sub_result = self.state.nostr_client.subscribe_with_id(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                filter,
                None,
            ) => {
                match sub_result {
                    Ok(output) if output.success.contains(&main_relay_url) => {
                        info!("Queued notification subscription for the main relay");
                    }
                    Ok(output) => {
                        error!(failed = ?output.failed, "Failed to enqueue notification subscription for the main relay");
                        return Err(ServiceError::Internal(
                            "Notification subscription was not queued for the main relay".to_string(),
                        ));
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
                return Ok(false);
            }
            sub_result = self.state.nostr_client.subscribe_with_id(
                SubscriptionId::new(NOTIFY_LIST_SUBSCRIPTION_ID),
                notify_filter,
                None,
            ) => {
                match sub_result {
                    Ok(output) if output.success.contains(&main_relay_url) => {
                        info!("Queued notify-list subscription for the main relay");
                    }
                    Ok(output) => {
                        error!(failed = ?output.failed, "Failed to enqueue notify-list subscription for the main relay");
                        return Err(ServiceError::Internal(
                            "Notify-list subscription was not queued for the main relay".to_string(),
                        ));
                    }
                    Err(e) => {
                        error!("Failed to subscribe to notify lists: {}", e);
                        return Err(e.into());
                    }
                }
            }
        }

        Ok(true)
    }

    async fn process_live_events(
        &self,
        event_tx: Sender<(Box<Event>, EventContext)>,
        service_pubkey: PublicKey,
        token: CancellationToken,
    ) -> Result<()> {
        let notifications = self.state.nostr_client.notifications();

        info!("Processing live events...");

        run_live_loop(
            notifications,
            event_tx,
            service_pubkey,
            token,
            Duration::from_secs(self.state.settings.nostr.event_silence_timeout_secs),
            self,
        )
        .await
    }
}

#[async_trait]
trait SubscriptionRecovery: Send + Sync {
    async fn resubscribe(&self, since: Timestamp, token: &CancellationToken) -> Result<bool>;
}

#[async_trait]
impl SubscriptionRecovery for NostrListener {
    async fn resubscribe(&self, since: Timestamp, token: &CancellationToken) -> Result<bool> {
        self.subscribe_to_live_events(token, since).await
    }
}

#[derive(Debug)]
struct ClosedRecovery {
    trigger_id: SubscriptionId,
    started_at: Instant,
    sibling_closed: bool,
    flow_resumed: bool,
}

impl ClosedRecovery {
    fn healthy_grace_elapsed(&self) -> bool {
        self.flow_resumed && self.started_at.elapsed() >= CLOSED_RECOVERY_GRACE
    }
}

/// Forward live relay notifications to the event handler until cancelled or
/// until no further notification can arrive.
///
/// Split out of [`NostrListener::process_live_events`] so the loop can be
/// driven directly in tests. Everything below depends only on the broadcast
/// receiver, not on a connected relay pool, so a plain `broadcast::channel`
/// exercises the same code the service runs.
async fn run_live_loop(
    mut notifications: broadcast::Receiver<RelayPoolNotification>,
    event_tx: Sender<(Box<Event>, EventContext)>,
    service_pubkey: PublicKey,
    token: CancellationToken,
    silence_timeout: Duration,
    recovery: &dyn SubscriptionRecovery,
) -> Result<()> {
    let mut recovery_attempted = false;
    let mut closed_recovery: Option<ClosedRecovery> = None;
    let mut closed_recovery_attempts = VecDeque::new();
    let mut silence_deadline = Instant::now() + silence_timeout;

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Cancellation received, shutting down");
                break;
            }

            _ = tokio::time::sleep_until(silence_deadline) => {
                if recovery_attempted {
                    return Err(ServiceError::Internal(format!(
                        "Nostr event flow did not resume within {:?} after resubscribing",
                        silence_timeout
                    )));
                }

                warn!(?silence_timeout, "Nostr event flow is silent; resubscribing to the main relay");
                if !recovery
                    .resubscribe(live_subscription_since(), &token)
                    .await?
                {
                    break;
                }
                recovery_attempted = true;
                silence_deadline = Instant::now() + silence_timeout;
            }

            res = notifications.recv() => {
                match res {
                    Ok(notification) => {
                        match notification {
                            RelayPoolNotification::Event { event, .. } => {
                                crate::metrics::event_received();

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

                                        recovery_attempted = false;
                                        silence_deadline = Instant::now() + silence_timeout;
                                        if let Some(state) = closed_recovery.as_mut() {
                                            state.flow_resumed = true;
                                        }
                                    }
                                }
                            }
                            RelayPoolNotification::Message { relay_url, message } => {
                                if let RelayMessage::Closed { subscription_id, message: reason } = &message {
                                    if is_owned_subscription(subscription_id.as_ref()) {
                                        if closed_recovery
                                            .as_ref()
                                            .is_some_and(ClosedRecovery::healthy_grace_elapsed)
                                        {
                                            closed_recovery = None;
                                        }

                                        if let Some(state) = closed_recovery.as_mut() {
                                            let is_stale_sibling = state.started_at.elapsed()
                                                < CLOSED_RECOVERY_GRACE
                                                && subscription_id.as_ref() != &state.trigger_id
                                                && !state.sibling_closed;
                                            if is_stale_sibling {
                                                state.sibling_closed = true;
                                                info!(%relay_url, subscription_id = %subscription_id, "Ignoring sibling closure queued before subscription recovery");
                                                continue;
                                            }

                                            return Err(ServiceError::Internal(format!(
                                                "Relay closed recovered subscription {subscription_id}"
                                            )));
                                        }

                                        if recovery_attempted {
                                            return Err(ServiceError::Internal(format!(
                                                "Relay closed subscription {subscription_id} during event-flow recovery"
                                            )));
                                        }

                                        let now = Instant::now();
                                        closed_recovery_attempts.retain(|started_at| {
                                            now.duration_since(*started_at) < silence_timeout
                                        });
                                        if closed_recovery_attempts.len()
                                            >= MAX_CLOSED_RECOVERIES_PER_WINDOW
                                        {
                                            return Err(ServiceError::Internal(format!(
                                                "Relay repeatedly closed live subscriptions: {} recoveries within {:?}",
                                                closed_recovery_attempts.len(),
                                                silence_timeout
                                            )));
                                        }

                                        warn!(%relay_url, subscription_id = %subscription_id, ?message, "Relay closed a live subscription; resubscribing");
                                        if MachineReadablePrefix::parse(reason)
                                            == Some(MachineReadablePrefix::RateLimited)
                                        {
                                            warn!(?RATE_LIMIT_RECOVERY_BACKOFF, "Relay rate-limited a live subscription; backing off before recovery");
                                            tokio::select! {
                                                biased;
                                                _ = token.cancelled() => break,
                                                _ = tokio::time::sleep(RATE_LIMIT_RECOVERY_BACKOFF) => {}
                                            }
                                        }

                                        if !recovery
                                            .resubscribe(live_subscription_since(), &token)
                                            .await?
                                        {
                                            break;
                                        }
                                        closed_recovery_attempts.push_back(now);
                                        recovery_attempted = true;
                                        closed_recovery = Some(ClosedRecovery {
                                            trigger_id: subscription_id.as_ref().clone(),
                                            started_at: Instant::now(),
                                            sibling_closed: false,
                                            flow_resumed: false,
                                        });
                                        silence_deadline = Instant::now() + silence_timeout;
                                        continue;
                                    }
                                }

                                match message {
                                    RelayMessage::Closed { .. } | RelayMessage::Notice(_) => {
                                        info!(%relay_url, ?message, "Received relay message");
                                    }
                                    _ => {
                                        debug!(%relay_url, ?message, "Received relay message");
                                    }
                                }
                            }
                            RelayPoolNotification::Shutdown => {
                                info!("Received shutdown notification");
                                break;
                            }
                        }
                    }
                    // `Lagged` means this subscriber fell behind a bounded
                    // broadcast channel and some notifications were dropped;
                    // the next `recv()` returns the oldest message still
                    // retained, so the listener must keep going. Treating it
                    // as fatal is what turned a moment of back-pressure into a
                    // permanent, silent outage: the loop ended, nothing
                    // restarted it, and `/health` stayed green.
                    Err(RecvError::Lagged(dropped)) => {
                        warn!(
                            dropped,
                            "Relay notification receiver lagged - dropped events, continuing"
                        );
                        continue;
                    }
                    // Every sender is gone; no further notification can arrive.
                    Err(RecvError::Closed) => {
                        error!("Relay notification channel closed - stopping live listener");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{CriticalTask, OnReturn, TaskHealth, TaskTracker};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;
    use tokio::time::{advance, timeout};

    /// A test's patience for the loop making progress. Generous enough not to
    /// flake on a loaded CI box, short enough to fail instead of hanging.
    const PATIENCE: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct MockRecovery {
        calls: AtomicUsize,
        since: Mutex<Vec<Timestamp>>,
    }

    #[async_trait]
    impl SubscriptionRecovery for MockRecovery {
        async fn resubscribe(&self, since: Timestamp, _token: &CancellationToken) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.since.lock().expect("since lock").push(since);
            Ok(true)
        }
    }

    fn live_event(keys: &Keys, content: &str) -> RelayPoolNotification {
        RelayPoolNotification::Event {
            relay_url: RelayUrl::parse("wss://relay.example").expect("valid relay url"),
            subscription_id: SubscriptionId::new("test"),
            event: Box::new(
                EventBuilder::new(Kind::TextNote, content)
                    .sign_with_keys(keys)
                    .expect("sign test event"),
            ),
        }
    }

    fn relay_message(message: RelayMessage<'static>) -> RelayPoolNotification {
        RelayPoolNotification::Message {
            relay_url: RelayUrl::parse("wss://relay.example").expect("valid relay url"),
            message,
        }
    }

    /// Regression: a lagged receive must not end the live loop.
    ///
    /// Overflows the notification channel so the loop's very first `recv()`
    /// returns `Lagged`, then asserts the notifications still retained behind
    /// it are forwarded anyway. Before the fix the loop broke on that first
    /// error and nothing restarted it, so every later push was lost while
    /// `/health` kept returning 200.
    #[tokio::test]
    async fn a_lagged_receive_does_not_end_the_live_loop() {
        let (notify_tx, notify_rx) = broadcast::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let keys = Keys::generate();

        // Four notifications into a two-slot channel with nobody reading yet:
        // the two oldest are evicted, so the receiver is now lagged by two.
        for content in ["dropped-1", "dropped-2", "retained-1", "retained-2"] {
            notify_tx.send(live_event(&keys, content)).expect("send");
        }

        let loop_handle = tokio::spawn(async move {
            let recovery = MockRecovery::default();
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                CancellationToken::new(),
                Duration::from_secs(60),
                &recovery,
            )
            .await
        });

        let first = timeout(PATIENCE, event_rx.recv())
            .await
            .expect("loop stalled after the lag")
            .expect("loop stopped on the lag instead of continuing");
        let second = timeout(PATIENCE, event_rx.recv())
            .await
            .expect("loop stalled after the first event")
            .expect("loop stopped after one event");

        assert_eq!(first.0.content, "retained-1");
        assert_eq!(second.0.content, "retained-2");

        // Dropping the last sender closes the channel, which *is* fatal - the
        // loop must then finish rather than spin on the error forever.
        drop(notify_tx);
        timeout(PATIENCE, loop_handle)
            .await
            .expect("loop did not stop once the channel closed")
            .expect("live loop panicked")
            .expect("closed channel should stop cleanly");
    }

    /// The other half of the same decision: `Closed` really is terminal, so
    /// "keep going on any error" is not a valid way to fix the lag bug.
    #[tokio::test]
    async fn a_closed_channel_ends_the_live_loop() {
        let (notify_tx, notify_rx) = broadcast::channel::<RelayPoolNotification>(2);
        let (event_tx, _event_rx) = mpsc::channel(8);

        drop(notify_tx);

        timeout(
            PATIENCE,
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                CancellationToken::new(),
                Duration::from_secs(60),
                &MockRecovery::default(),
            ),
        )
        .await
        .expect("loop spun on a closed channel instead of stopping")
        .expect("closed channel should stop cleanly");
    }

    /// Cancellation still wins over a pending receive.
    #[tokio::test]
    async fn cancellation_ends_the_live_loop() {
        let (_notify_tx, notify_rx) = broadcast::channel::<RelayPoolNotification>(2);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        token.cancel();

        timeout(
            PATIENCE,
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token,
                Duration::from_secs(60),
                &MockRecovery::default(),
            ),
        )
        .await
        .expect("loop ignored cancellation")
        .expect("cancellation should stop cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn silent_subscription_resubscribes_then_fails_listener_health() {
        let (_notify_tx, notify_rx) = broadcast::channel::<RelayPoolNotification>(2);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let silence_timeout = Duration::from_secs(30);
        let recovery = Arc::new(MockRecovery::default());
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        let recovery_for_task = Arc::clone(&recovery);
        tracker.spawn(
            "nostr_listener",
            Some(CriticalTask::NostrListener),
            OnReturn::Fatal,
            async move {
                let _ = run_live_loop(
                    notify_rx,
                    event_tx,
                    Keys::generate().public_key(),
                    token,
                    silence_timeout,
                    recovery_for_task.as_ref(),
                )
                .await;
            },
        );

        tokio::task::yield_now().await;
        advance(silence_timeout).await;
        tokio::task::yield_now().await;

        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);
        let since = recovery.since.lock().expect("since lock")[0];
        let lookback = Timestamp::now().as_secs() - since.as_secs();
        assert!(
            (LIVE_EVENT_LOOKBACK.as_secs() - 1..=LIVE_EVENT_LOOKBACK.as_secs() + 1)
                .contains(&lookback),
            "recovery must replay the one-hour window, got {lookback} seconds"
        );
        assert!(health.is_alive(CriticalTask::NostrListener));

        advance(silence_timeout).await;
        tracker.wait().await;

        assert!(!health.is_alive(CriticalTask::NostrListener));
        assert!(health.had_unexpected_exit());
    }

    /// Resumed event flow must rearm the watchdog after a silence recovery.
    ///
    /// This is what keeps an ordinary quiet spell from compounding into a
    /// restart. The first silent window resubscribes and arms
    /// `recovery_attempted`; once events reach the handler again that flag has
    /// to clear, so the *next* silent window resubscribes as well instead of
    /// taking the failure branch. The test above only covers the opposite
    /// direction, where flow never resumes and the listener is meant to die.
    #[tokio::test(start_paused = true)]
    async fn resumed_flow_rearms_the_watchdog_after_a_silence_recovery() {
        let (notify_tx, notify_rx) = broadcast::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();
        let silence_timeout = Duration::from_secs(30);

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                silence_timeout,
                recovery_for_task.as_ref(),
            )
            .await
        });

        tokio::task::yield_now().await;
        advance(silence_timeout).await;
        tokio::task::yield_now().await;
        assert_eq!(
            recovery.calls.load(Ordering::SeqCst),
            1,
            "the first silent window must resubscribe"
        );

        notify_tx
            .send(live_event(&Keys::generate(), "flow resumed"))
            .expect("send live event");
        event_rx.recv().await.expect("forward the resumed event");

        advance(silence_timeout).await;
        tokio::task::yield_now().await;
        assert!(
            !loop_handle.is_finished(),
            "resumed flow must clear the recovery flag, not leave the listener one quiet window from exit"
        );
        assert_eq!(
            recovery.calls.load(Ordering::SeqCst),
            2,
            "a later silent window must resubscribe again rather than fail the listener"
        );

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("cancellation should stop cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn our_closed_subscription_resubscribes_immediately() {
        let (notify_tx, notify_rx) = broadcast::channel(2);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();

        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "error: closed by relay",
            )))
            .expect("send CLOSED");

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                Duration::from_secs(30),
                recovery_for_task.as_ref(),
            )
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("cancellation should stop cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_closed_subscription_does_not_resubscribe() {
        let (notify_tx, notify_rx) = broadcast::channel(2);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();

        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new("another-component"),
                "error: closed by relay",
            )))
            .expect("send CLOSED");

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                Duration::from_secs(30),
                recovery_for_task.as_ref(),
            )
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 0);

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("cancellation should stop cleanly");
    }

    /// A second closure of an already-recovered subscription must fail the
    /// listener rather than replay again.
    ///
    /// The closure reason deliberately carries no `rate-limited:` prefix. That
    /// prefix would add a real five-second `RATE_LIMIT_RECOVERY_BACKOFF` sleep
    /// to a test bounded by an equally long `PATIENCE`, leaving the assertion
    /// to race its own timeout. The backoff has its own test below.
    #[tokio::test]
    async fn a_second_owned_closed_message_fails_instead_of_replaying_again() {
        let (notify_tx, notify_rx) = broadcast::channel(4);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let recovery = MockRecovery::default();

        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "error: closed by relay",
            )))
            .expect("send first CLOSED");
        notify_tx
            .send(live_event(&Keys::generate(), "traffic between closures"))
            .expect("send event");
        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "error: closed by relay",
            )))
            .expect("send second CLOSED");

        let error = timeout(
            PATIENCE,
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                CancellationToken::new(),
                Duration::from_secs(60),
                &recovery,
            ),
        )
        .await
        .expect("live loop did not react to repeated CLOSED")
        .expect_err("a repeated owned CLOSED must fail the listener");

        assert!(error.to_string().contains(NOTIFICATION_SUBSCRIPTION_ID));
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sibling_closures_trigger_one_recovery() {
        let (notify_tx, notify_rx) = broadcast::channel(4);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();

        for subscription_id in [NOTIFICATION_SUBSCRIPTION_ID, NOTIFY_LIST_SUBSCRIPTION_ID] {
            notify_tx
                .send(relay_message(RelayMessage::closed(
                    SubscriptionId::new(subscription_id),
                    "error: relay restart",
                )))
                .expect("send CLOSED");
        }

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                Duration::from_secs(60),
                recovery_for_task.as_ref(),
            )
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("stale sibling closure must not fail recovery");
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_flow_allows_a_later_closed_recovery() {
        let (notify_tx, notify_rx) = broadcast::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                Duration::from_secs(60),
                recovery_for_task.as_ref(),
            )
            .await
        });

        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "error: first closure",
            )))
            .expect("send first CLOSED");
        notify_tx
            .send(live_event(&Keys::generate(), "healthy traffic"))
            .expect("send live event");
        event_rx.recv().await.expect("forward healthy event");
        advance(CLOSED_RECOVERY_GRACE).await;
        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "error: later closure",
            )))
            .expect("send later CLOSED");
        tokio::task::yield_now().await;

        assert_eq!(recovery.calls.load(Ordering::SeqCst), 2);

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("healthy flow must reset closed recovery");
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_closed_recoveries_fail_listener_health() {
        let (notify_tx, notify_rx) = broadcast::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let recovery = Arc::new(MockRecovery::default());
        let silence_timeout = Duration::from_secs(60);

        let recovery_for_task = Arc::clone(&recovery);
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                CancellationToken::new(),
                silence_timeout,
                recovery_for_task.as_ref(),
            )
            .await
        });

        for attempt in 0..MAX_CLOSED_RECOVERIES_PER_WINDOW {
            notify_tx
                .send(relay_message(RelayMessage::closed(
                    SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                    "rate-limited: slow down",
                )))
                .expect("send CLOSED");
            tokio::task::yield_now().await;
            advance(RATE_LIMIT_RECOVERY_BACKOFF).await;
            tokio::task::yield_now().await;
            assert_eq!(recovery.calls.load(Ordering::SeqCst), attempt + 1);

            notify_tx
                .send(live_event(&Keys::generate(), "trickle traffic"))
                .expect("send trickle event");
            event_rx.recv().await.expect("forward trickle event");
            advance(CLOSED_RECOVERY_GRACE).await;
        }

        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "rate-limited: slow down",
            )))
            .expect("send one closure beyond the recovery budget");

        let error = loop_handle
            .await
            .expect("live loop panicked")
            .expect_err("repeated CLOSED recovery must fail listener health");
        assert!(error.to_string().contains("3 recoveries within 60s"));
        assert_eq!(
            recovery.calls.load(Ordering::SeqCst),
            MAX_CLOSED_RECOVERIES_PER_WINDOW,
            "the over-budget closure must fail before another replay"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_closure_backs_off_before_recovery() {
        let (notify_tx, notify_rx) = broadcast::channel(2);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();

        notify_tx
            .send(relay_message(RelayMessage::closed(
                SubscriptionId::new(NOTIFICATION_SUBSCRIPTION_ID),
                "rate-limited: slow down",
            )))
            .expect("send CLOSED");

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                Duration::from_secs(60),
                recovery_for_task.as_ref(),
            )
            .await
        });

        tokio::task::yield_now().await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 0);
        advance(RATE_LIMIT_RECOVERY_BACKOFF - Duration::from_secs(1)).await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 0);
        advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("cancellation should stop cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn downstream_backpressure_does_not_trigger_recovery() {
        let (notify_tx, notify_rx) = broadcast::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let recovery = Arc::new(MockRecovery::default());
        let token = CancellationToken::new();
        let keys = Keys::generate();
        let queued = live_event(&keys, "already queued");
        let RelayPoolNotification::Event { event, .. } = queued else {
            unreachable!("live_event always constructs an event")
        };
        event_tx
            .send((event, EventContext::Live))
            .await
            .expect("pre-fill event channel");

        notify_tx
            .send(live_event(&keys, "blocked by downstream"))
            .expect("send live event");

        let recovery_for_task = Arc::clone(&recovery);
        let token_for_task = token.clone();
        let loop_handle = tokio::spawn(async move {
            run_live_loop(
                notify_rx,
                event_tx,
                Keys::generate().public_key(),
                token_for_task,
                Duration::from_secs(30),
                recovery_for_task.as_ref(),
            )
            .await
        });

        tokio::task::yield_now().await;
        advance(Duration::from_secs(30)).await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 0);

        event_rx.recv().await.expect("drain pre-filled event");
        event_rx.recv().await.expect("forward blocked event");
        advance(Duration::from_secs(29)).await;
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 0);

        token.cancel();
        loop_handle
            .await
            .expect("live loop panicked")
            .expect("cancellation should stop cleanly");
    }

    #[test]
    fn only_service_owned_subscription_ids_trigger_recovery() {
        assert!(is_owned_subscription(&SubscriptionId::new(
            NOTIFICATION_SUBSCRIPTION_ID
        )));
        assert!(is_owned_subscription(&SubscriptionId::new(
            NOTIFY_LIST_SUBSCRIPTION_ID
        )));
        assert!(!is_owned_subscription(&SubscriptionId::new(
            "another-component"
        )));
    }
}
