//! Event handler for diVine Push Service
//!
//! Handles Nostr events and routes them to appropriate notification handlers.
//! Supports:
//! - Token registration/deregistration (kinds 3079/3080)
//! - Notification types: likes, comments, mentions, reposts, and new posts

use crate::{
    crypto::CryptoService,
    error::Result,
    fcm_sender,
    models::FcmPayload,
    preferences::{self, NotificationType, UserPreferences},
    redis_store,
    state::AppState,
};
use futures_util::{stream, StreamExt};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, trace, warn};

/// Context for event processing to distinguish historical from live events
#[derive(Debug, Clone, Copy)]
pub enum EventContext {
    /// Historical event being replayed (e.g., during startup or reconnection)
    Historical,
    /// Live event received in real-time
    Live,
}

// Control event kinds for push notification management
const KIND_REGISTRATION: u16 = 3079;
const KIND_DEREGISTRATION: u16 = 3080;
const KIND_PREFERENCES_UPDATE: u16 = 3083;
const KIND_VIDEO: u16 = 34236;
const MAX_FANOUT_ATTEMPTS: u16 = 12;

/// NIP-51 people list carrying new-post ("bell") subscriptions.
const KIND_NOTIFY_LIST: u16 = 30000;

/// Reserved `d` tag identifying a notify list among the user's kind 30000 lists.
pub const NOTIFY_LIST_D_TAG: &str = "notify";

// Replay horizon: ignore events older than this
const REPLAY_HORIZON_DAYS: u64 = 7;

/// Whether this event is a notify list, which is exempt from the replay horizon.
///
/// Notify lists are replaceable: a list published three months ago and never
/// touched since is still the user's current subscription set. Aging one out
/// would silently drop every bell on it until the user happened to republish.
///
/// Checks the `d` tag as well as the kind, for the same reason
/// `handle_notify_list_update` does: the relay-side `#d` filter should already
/// guarantee it, but a buggy or hostile relay can send any kind 30000 it likes,
/// and the exemption should cover exactly the events it exists for.
pub fn is_notify_list(event: &Event) -> bool {
    event.kind.as_u16() == KIND_NOTIFY_LIST && event.tags.identifier() == Some(NOTIFY_LIST_D_TAG)
}

/// Whether the handler loop should drop this event as beyond the replay horizon.
///
/// Extracted from `run()` so the exemption is covered by a test. Getting this
/// composition wrong is silent: bells set more than `REPLAY_HORIZON_DAYS` ago
/// simply never rebuild, and nothing logs an error.
pub fn is_beyond_replay_horizon(event: &Event) -> bool {
    is_event_too_old(event) && !is_notify_list(event)
}

/// Whether the handler loop should claim this event before routing it.
///
/// Control events mutate one event-scoped record and retain the coarse claim.
/// Content delivery instead claims `(event_id, recipient)` immediately before
/// FCM, so a failed or interrupted recipient cannot consume every recipient
/// behind it. Notify lists remain idempotent through their atomic replacement
/// script, and unrelated event kinds do no work here.
pub fn requires_event_claim(event: &Event) -> bool {
    matches!(
        event.kind.as_u16(),
        KIND_REGISTRATION | KIND_DEREGISTRATION | KIND_PREFERENCES_UPDATE
    )
}

/// Check if event is targeted to this service via p tag
fn is_event_for_service(event: &Event, service_pubkey: &PublicKey) -> bool {
    event
        .tags
        .iter()
        .filter(|t| t.kind() == TagKind::p())
        .filter_map(|t| t.content())
        .any(|pubkey_str| {
            PublicKey::from_str(pubkey_str)
                .map(|pk| pk == *service_pubkey)
                .unwrap_or(false)
        })
}

/// Check if an event is too old based on the replay horizon
pub fn is_event_too_old(event: &Event) -> bool {
    use std::time::Duration;

    let horizon = Timestamp::now() - Duration::from_secs(REPLAY_HORIZON_DAYS * 24 * 60 * 60);
    event.created_at < horizon
}

/// Main event handler loop
pub async fn run(
    state: Arc<AppState>,
    mut event_rx: Receiver<(Box<Event>, EventContext)>,
    token: CancellationToken,
) -> Result<()> {
    info!("Starting diVine event handler...");

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Event handler cancellation received. Shutting down...");
                break;
            }

            maybe_event = event_rx.recv() => {
                let Some((event, context)) = maybe_event else {
                    info!("Event channel closed. Event handler shutting down.");
                    break;
                };

                let event_id = event.id;
                let event_kind = event.kind;
                let pubkey = event.pubkey;

                debug!(event_id = %event_id, kind = %event_kind, pubkey = %pubkey, context = ?context, "Event handler received event");

                // Check replay horizon - ignore events that are too old.
                // Notify lists are exempt: they are replaceable subscription
                // state, not a timely trigger, so age says nothing about
                // whether they are current.
                if is_beyond_replay_horizon(&event) {
                    debug!(event_id = %event_id, created_at = %event.created_at, "Ignoring old event beyond replay horizon");
                    continue;
                }

                // Atomically claim the event to prevent duplicate processing across
                // replicas. Notify lists are exempt: the claim prevents nothing for
                // them and turns a transient Redis error into days of lost
                // subscriptions. See `requires_event_claim`.
                if requires_event_claim(&event) {
                    let claimed = tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            info!("Event handler cancelled while claiming event {}.", event_id);
                            break;
                        }
                        claim_result = redis_store::try_claim_event(
                            &state.redis_pool,
                            &event_id,
                            state.settings.service.processed_event_ttl_secs,
                        ) => {
                            match claim_result {
                                Ok(claimed) => claimed,
                                Err(e) => {
                                    error!(event_id = %event_id, error = %e, "Failed to claim event");
                                    continue;
                                }
                            }
                        }
                    };

                    if !claimed {
                        trace!(event_id = %event_id, "Skipping already claimed event");
                        continue;
                    }
                }

                // Route the event based on its type
                let handler_result = route_event(&state, &event, context, token.clone()).await;

                match handler_result {
                    Ok(_) => {
                        trace!(event_id = %event_id, kind = %event_kind, "Handler finished successfully");
                    }
                    Err(e) => {
                        error!(event_id = %event_id, error = %e, "Failed to handle event");
                    }
                }

                crate::metrics::event_processed();

                if token.is_cancelled() {
                    info!(event_id = %event_id, "Event handler cancellation detected after processing event.");
                    break;
                }
            }
        }
    }

    info!("Event handler shut down.");
    Ok(())
}

/// Route event to appropriate handler based on event kind
async fn route_event(
    state: &AppState,
    event: &Event,
    context: EventContext,
    token: CancellationToken,
) -> Result<()> {
    let event_kind = event.kind;
    let event_id = event.id;
    let kind_num = event_kind.as_u16();

    // Notify lists are addressed to the world, not to this service, so they are
    // routed before the control-event block and deliberately outside its p-tag
    // gate. They are also processed in both the historical and live paths: the
    // reverse index must be rebuilt from history or a restart drops every
    // subscription until each user happens to republish.
    if kind_num == KIND_NOTIFY_LIST {
        return handle_notify_list_update(state, event).await;
    }

    // Check for push notification management events (3079/3080/3083)
    let is_control_event = kind_num == KIND_REGISTRATION
        || kind_num == KIND_DEREGISTRATION
        || kind_num == KIND_PREFERENCES_UPDATE;

    if is_control_event {
        debug!(event_id = %event_id, kind = %event_kind, "Processing control event");

        // Check if this event is targeted to our service via p tag
        if let Some(ref service_keys) = state.service_keys {
            if !is_event_for_service(event, &service_keys.public_key()) {
                debug!(event_id = %event_id, kind = %event_kind, "Ignoring control event not targeted to our service");
                return Ok(());
            }
            debug!(event_id = %event_id, "Control event is for this service");
        } else {
            warn!("No service keys configured - cannot filter by p tag");
        }

        // Route to appropriate control handler
        if kind_num == KIND_REGISTRATION {
            return handle_registration(state, event).await;
        } else if kind_num == KIND_DEREGISTRATION {
            return handle_deregistration(state, event).await;
        } else if kind_num == KIND_PREFERENCES_UPDATE {
            return handle_preferences_update(state, event).await;
        } else {
            return Ok(());
        }
    }

    // Skip notification processing for historical events
    if matches!(context, EventContext::Historical) {
        debug!(event_id = %event_id, "Skipping notification processing for historical event");
        return Ok(());
    }

    // Handle content events that may trigger notifications
    handle_content_event(state, event, token).await
}

/// Handle token registration (kind 3079)
async fn handle_registration(state: &AppState, event: &Event) -> Result<()> {
    assert!(event.kind.as_u16() == KIND_REGISTRATION);

    // Validate that content is NIP-44 encrypted
    if let Err(e) = CryptoService::validate_encrypted_content(&event.content) {
        error!(
            event_id = %event.id, pubkey = %event.pubkey, error = %e,
            "Received registration with plaintext token - rejecting"
        );
        return Ok(()); // Don't process plaintext tokens
    }

    // Get crypto service from state
    let crypto_service = match &state.crypto_service {
        Some(service) => service,
        None => {
            error!(event_id = %event.id, "No crypto service configured - cannot decrypt tokens");
            return Ok(());
        }
    };

    // Decrypt the NIP-44 content
    let token_payload = match crypto_service.decrypt_token_payload(&event.content, &event.pubkey) {
        Ok(payload) => payload,
        Err(e) => {
            error!(
                event_id = %event.id, pubkey = %event.pubkey, error = %e,
                "Failed to decrypt registration token"
            );
            return Ok(()); // Don't fail the whole handler for decryption errors
        }
    };

    let fcm_token = token_payload.token.trim();
    if fcm_token.is_empty() {
        warn!(
            event_id = %event.id, pubkey = %event.pubkey,
            "Received registration event with empty token after decryption"
        );
        return Ok(());
    }

    match redis_store::add_or_update_token(&state.redis_pool, &event.pubkey, fcm_token).await {
        Ok(_) => {
            info!(event_id = %event.id, pubkey = %event.pubkey, "Registered/Updated encrypted token");
        }
        Err(e) => {
            return Err(e);
        }
    }
    Ok(())
}

/// Handle token deregistration (kind 3080)
async fn handle_deregistration(state: &AppState, event: &Event) -> Result<()> {
    assert!(event.kind.as_u16() == KIND_DEREGISTRATION);

    // Validate that content is NIP-44 encrypted
    if let Err(e) = CryptoService::validate_encrypted_content(&event.content) {
        error!(
            event_id = %event.id, pubkey = %event.pubkey, error = %e,
            "Received deregistration with plaintext token - rejecting"
        );
        return Ok(()); // Don't process plaintext tokens
    }

    // Get crypto service from state
    let crypto_service = match &state.crypto_service {
        Some(service) => service,
        None => {
            error!(event_id = %event.id, "No crypto service configured - cannot decrypt tokens");
            return Ok(());
        }
    };

    // Decrypt the NIP-44 content
    let token_payload = match crypto_service.decrypt_token_payload(&event.content, &event.pubkey) {
        Ok(payload) => payload,
        Err(e) => {
            error!(
                event_id = %event.id, pubkey = %event.pubkey, error = %e,
                "Failed to decrypt deregistration token"
            );
            return Ok(()); // Don't fail the whole handler for decryption errors
        }
    };

    let fcm_token = token_payload.token.trim();
    if fcm_token.is_empty() {
        warn!(
            event_id = %event.id, pubkey = %event.pubkey,
            "Received deregistration event with empty token after decryption"
        );
        return Ok(());
    }

    let removed = redis_store::remove_token(&state.redis_pool, &event.pubkey, fcm_token).await?;
    if removed {
        info!(event_id = %event.id, pubkey = %event.pubkey, "Deregistered encrypted token");
    } else {
        debug!(
            event_id = %event.id, pubkey = %event.pubkey,
            "Token not found for deregistration"
        );
    }

    // Preferences are identity-scoped, while deregistration removes one device token.
    // Keep the account's choices so signing out or switching accounts cannot reset them.

    Ok(())
}

/// Handle preferences update (kind 3083)
async fn handle_preferences_update(state: &AppState, event: &Event) -> Result<()> {
    assert!(event.kind.as_u16() == KIND_PREFERENCES_UPDATE);

    // Validate that content is NIP-44 encrypted
    if let Err(e) = CryptoService::validate_encrypted_content(&event.content) {
        error!(
            event_id = %event.id, pubkey = %event.pubkey, error = %e,
            "Received preferences update with plaintext content - rejecting"
        );
        return Ok(());
    }

    // Get crypto service from state
    let crypto_service = match &state.crypto_service {
        Some(service) => service,
        None => {
            error!(event_id = %event.id, "No crypto service configured - cannot decrypt preferences");
            return Ok(());
        }
    };

    // Decrypt the NIP-44 content
    let decrypted = match crypto_service.decrypt_nip44(&event.content, &event.pubkey) {
        Ok(content) => content,
        Err(e) => {
            error!(
                event_id = %event.id, pubkey = %event.pubkey, error = %e,
                "Failed to decrypt preferences update"
            );
            return Ok(());
        }
    };

    // Parse preferences from decrypted content
    let prefs: UserPreferences = match serde_json::from_str(&decrypted) {
        Ok(p) => p,
        Err(e) => {
            error!(
                event_id = %event.id, pubkey = %event.pubkey, error = %e,
                "Failed to parse preferences JSON"
            );
            return Ok(());
        }
    };

    // Store preferences
    let pubkey_hex = event.pubkey.to_hex();
    preferences::set_user_preferences(&state.redis_pool, &pubkey_hex, &prefs).await?;

    info!(event_id = %event.id, pubkey = %event.pubkey, prefs = ?prefs, "Updated user preferences");

    Ok(())
}

/// Extract the subscribed creators from a notify list, deduplicated.
///
/// Drops self-references (belling yourself is meaningless) and unparseable `p`
/// tags, preserving the client's tag order otherwise.
///
/// Bounded at `max_creators`. `replace_notify_subscriptions` applies the whole
/// diff in a single Lua script and Redis is single-threaded, so an unbounded
/// list would let one user with an absurd number of bells stall the instance for
/// everyone. Truncating rather than rejecting degrades gracefully: the excess
/// bells simply do not deliver, instead of the user losing all of them. `p` tags
/// are ordered by the client, so which ones survive is deterministic and under
/// the client's control.
fn collect_notify_creators(event: &Event, max_creators: usize) -> Vec<PublicKey> {
    let mut creators: Vec<PublicKey> = Vec::new();

    for tag in event.tags.iter().filter(|t| t.kind() == TagKind::p()) {
        let Some(pubkey) = tag.content().and_then(|c| PublicKey::from_str(c).ok()) else {
            continue;
        };
        if pubkey == event.pubkey {
            continue;
        }
        if creators.contains(&pubkey) {
            continue;
        }
        if creators.len() == max_creators {
            warn!(
                event_id = %event.id,
                pubkey = %event.pubkey,
                max_creators,
                "Notify list exceeds the creator cap; ignoring the remainder"
            );
            break;
        }
        creators.push(pubkey);
    }

    creators
}

/// Handle a notify-list update (kind 30000, `d=notify`).
///
/// Rebuilds this subscriber's slice of the reverse index from the replacement
/// list. The list is public and unencrypted by design — the service has to be
/// able to read it — so there is no decryption step here, unlike the control
/// events above.
async fn handle_notify_list_update(state: &AppState, event: &Event) -> Result<()> {
    assert!(event.kind.as_u16() == KIND_NOTIFY_LIST);

    // Defense in depth. The relay-side `#d` filter should already guarantee
    // this, but a buggy or hostile relay can send any kind 30000 it likes, and
    // applying someone's unrelated people list as their bell list would be
    // both wrong and destructive.
    let d_tag = event.tags.identifier();
    if d_tag != Some(NOTIFY_LIST_D_TAG) {
        debug!(
            event_id = %event.id,
            pubkey = %event.pubkey,
            d_tag = ?d_tag,
            "Ignoring kind 30000 list without the reserved notify d tag"
        );
        return Ok(());
    }

    let creators = collect_notify_creators(event, state.settings.service.notify_list_max_creators);

    // An empty list is legitimate: the user unbelled everyone. It must clear
    // the index rather than be treated as malformed and skipped.
    let applied = redis_store::replace_notify_subscriptions(
        &state.redis_pool,
        &event.pubkey,
        &creators,
        event.created_at.as_secs(),
        &event.id,
    )
    .await?;

    if applied {
        info!(
            event_id = %event.id,
            pubkey = %event.pubkey,
            creator_count = creators.len(),
            "Applied notify list update"
        );
    } else {
        debug!(
            event_id = %event.id,
            pubkey = %event.pubkey,
            created_at = %event.created_at,
            "Ignoring notify list update older than the applied one"
        );
    }

    Ok(())
}

/// Handle content events that may trigger notifications
async fn handle_content_event(
    state: &AppState,
    event: &Event,
    token: CancellationToken,
) -> Result<()> {
    let event_id = event.id;
    let event_kind = event.kind;

    // Determine notification type and find recipients based on event kind
    let kind_num = event_kind.as_u16();

    let targets = if kind_num == 7 {
        // Kind 7: Reaction/Like - notify the author of the liked event
        targets_of(NotificationType::Like, find_reaction_recipients(event))
    } else if kind_num == 1 {
        // Kind 1: Text note - could be a comment or mention
        let recipients = find_text_note_recipients(event);
        // Determine if it's a comment (has e-tag) or mention (has p-tag only)
        let has_e_tag = event.tags.find(TagKind::e()).is_some();
        let notification_type = if has_e_tag {
            NotificationType::Comment
        } else {
            NotificationType::Mention
        };
        targets_of(notification_type, recipients)
    } else if kind_num == 1111 {
        // Kind 1111: NIP-22 comment (diVine publishes video comments here, not
        // as kind 1). Notify the root author (uppercase `P`, the video owner)
        // and the direct parent author (lowercase `p`). create_fcm_payload
        // attaches the authoritative root-video target from the uppercase `A`.
        targets_of(NotificationType::Comment, find_comment_recipients(event))
    } else if kind_num == 16 {
        // Kind 16: Repost - notify the author of the reposted event
        targets_of(NotificationType::Repost, find_repost_recipients(event))
    } else if kind_num == 30023 {
        // Kind 30023: Long-form content - check for mentions
        targets_of(NotificationType::Mention, find_mentioned_pubkeys(event))
    } else if kind_num == KIND_VIDEO {
        return handle_video_content_event(state, event, token).await;
    } else {
        trace!(event_id = %event_id, kind = %event_kind, "Ignoring event kind - no notification handler");
        return Ok(());
    };

    // Drop self-targets before anything downstream reads the list. Several of
    // the `find_*` helpers return the author when an event `p`-tags its own
    // sender, and the send loop used to skip those one at a time — which was
    // free while the payload was built per recipient, but now sits after the
    // event-scoped copy has been resolved. Filtering here keeps a self-only
    // event from paying for a profile lookup it can never use, and makes the
    // count and per-type breakdown logged below describe real deliveries.
    let targets = deliverable_targets(targets, &event.pubkey);

    if targets.is_empty() {
        debug!(event_id = %event_id, kind = %event_kind, "No recipients found for event");
        return Ok(());
    }

    // A single event can now yield more than one notification type, so the log
    // carries the per-type breakdown rather than one type for the whole event.
    // A bare count would leave the type most likely to need operational
    // attention -- NewPost, with its fan-out and rate limiting -- invisible.
    let mut type_counts: Vec<(&str, usize)> = Vec::new();
    for target in &targets {
        let name = target.notification_type.display_name();
        match type_counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => type_counts.push((name, 1)),
        }
    }

    info!(
        event_id = %event_id,
        kind = %event_kind,
        recipient_count = targets.len(),
        notification_types = ?type_counts,
        "Processing notification event"
    );

    // The recipient-independent copy resolves at most once for the event, and
    // only if some recipient survives the gates in `send_notification_to_user`.
    let copy = LazyEventCopy::for_targets(&targets);

    send_notifications_sequential(state, event, targets, &copy, token).await
}

async fn send_notifications_sequential(
    state: &AppState,
    event: &Event,
    targets: Vec<NotificationTarget>,
    copy: &LazyEventCopy,
    token: CancellationToken,
) -> Result<()> {
    let mut first_error = None;
    for target in targets {
        if token.is_cancelled() {
            info!(event_id = %event.id, "Notification sending cancelled");
            return Err(crate::error::ServiceError::Cancelled);
        }

        let recipient_pubkey = target.recipient;

        if let Err(e) = send_notification_to_user(
            state,
            event,
            &recipient_pubkey,
            target.notification_type,
            copy,
            token.clone(),
        )
        .await
        {
            if matches!(e, crate::error::ServiceError::Cancelled) {
                return Err(e);
            }
            error!(
                event_id = %event.id,
                recipient = %recipient_pubkey,
                error = %e,
                "Failed to send notification"
            );
            first_error.get_or_insert(e);
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn send_notifications_bounded(
    state: &AppState,
    event: &Event,
    targets: Vec<NotificationTarget>,
    copy: &LazyEventCopy,
    token: CancellationToken,
    concurrency: usize,
) -> Result<()> {
    let mut deliveries = stream::iter(targets)
        .map(|target| {
            let token = token.clone();
            async move {
                let recipient = target.recipient;
                (
                    recipient,
                    deliver_notification_target(state, event, target, copy, token).await,
                )
            }
        })
        .buffer_unordered(concurrency);

    // Drain on cancellation rather than returning at the first `Cancelled`.
    //
    // Returning early drops the `buffer_unordered` stream, and with it every
    // still-running delivery, at whatever await point it had reached. Up to
    // `concurrency - 1` of them can be sitting between the FCM send and the
    // bookkeeping that follows it: the rate-limit window, the coordinate claim,
    // invalid-token removal. Those pushes have already shipped, so dropping
    // them there double-pushes on restart and leaves tokens FCM has already
    // rejected registered — the same failure the post-send writes were made
    // log-and-continue to avoid, arriving through the other door. Sequential
    // delivery could strand one recipient this way; bounded delivery strands a
    // page's worth, and silently, because a dropped future logs nothing.
    //
    // Draining costs little on shutdown: every in-flight delivery holds the
    // same token and bails at its own next checkpoint, so the ones that have
    // not sent yet return immediately. It does not close the window inside
    // `send_notification_to_user`, which still returns `Cancelled` between the
    // FCM send and the writes below it; that one predates this branch.
    let mut first_error = None;
    while let Some((recipient, result)) = deliveries.next().await {
        if let Err(e) = result {
            if matches!(e, crate::error::ServiceError::Cancelled) {
                first_error.get_or_insert(e);
                continue;
            }
            error!(
                event_id = %event.id,
                recipient = %recipient,
                error = %e,
                "Failed to send notification"
            );
            first_error.get_or_insert(e);
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn deliver_notification_target(
    state: &AppState,
    event: &Event,
    target: NotificationTarget,
    copy: &LazyEventCopy,
    token: CancellationToken,
) -> Result<()> {
    let recipient_pubkey = target.recipient;

    // Skip if recipient is the sender
    if recipient_pubkey == event.pubkey {
        trace!(event_id = %event.id, "Skipping notification to sender");
        return Ok(());
    }

    send_notification_to_user(
        state,
        event,
        &recipient_pubkey,
        target.notification_type,
        copy,
        token,
    )
    .await
}

async fn handle_video_content_event(
    state: &AppState,
    event: &Event,
    token: CancellationToken,
) -> Result<()> {
    let mention_targets = deliverable_targets(video_mention_targets(event), &event.pubkey);
    // Mentions remain inline and small. The resolved sender name, when mentions
    // needed it, is carried into the durable watcher job so later pages do not
    // repeat profile lookup work.
    let copy = LazyEventCopy::for_targets(&mention_targets);

    let mention_result = if !mention_targets.is_empty() {
        info!(
            event_id = %event.id,
            kind = %event.kind,
            recipient_count = mention_targets.len(),
            notification_types = ?vec![(NotificationType::Mention.display_name(), mention_targets.len())],
            "Processing notification event"
        );
        send_notifications_sequential(state, event, mention_targets, &copy, token.clone()).await
    } else {
        Ok(())
    };

    let job = NewPostFanoutJob {
        event_json: serde_json::to_string(event)?,
        cursor: 0,
        sender_name: copy.cell.get().map(|resolved| resolved.sender_name.clone()),
        attempt: 0,
        expires_at: Timestamp::now()
            .as_secs()
            .saturating_add(state.settings.service.processed_event_ttl_secs),
    };
    let job_json = serde_json::to_string(&job)?;
    let enqueued = redis_store::enqueue_initial_fanout_job(
        &state.redis_pool,
        &event.id,
        &job_json,
        state.settings.service.processed_event_ttl_secs,
    )
    .await?;
    debug!(event_id = %event.id, enqueued, "Queued durable new-post fan-out");

    mention_result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NewPostFanoutJob {
    event_json: String,
    cursor: u64,
    sender_name: Option<String>,
    attempt: u16,
    expires_at: u64,
}

/// Runs durable cursor-paged new-post fan-out outside the event-handler loop.
pub async fn run_new_post_fanout(state: Arc<AppState>, token: CancellationToken) -> Result<()> {
    info!("Starting durable new-post fan-out worker...");
    let poll_interval = Duration::from_millis(state.settings.service.new_post_fanout_poll_millis);

    loop {
        if token.is_cancelled() {
            break;
        }

        let job = tokio::select! {
            biased;
            _ = token.cancelled() => break,
            result = redis_store::claim_fanout_job(
                &state.redis_pool,
                state.settings.service.new_post_fanout_lease_secs,
            ) => result,
        };

        match job {
            Ok(Some(job_json)) => {
                if let Err(e) = process_fanout_job(&state, &job_json, token.clone()).await {
                    if matches!(e, crate::error::ServiceError::Cancelled) {
                        if let Err(retry_error) =
                            redis_store::rescore_fanout_job(&state.redis_pool, &job_json, 0).await
                        {
                            error!(error = %retry_error, "Failed to release cancelled fan-out page lease; lease expiry will recover it");
                        }
                        break;
                    }

                    error!(error = %e, "Durable fan-out page processing failed; preserving the leased page");
                    // Keep the same member here: this path means the atomic
                    // attempt swap itself may have failed, so constructing a
                    // second swap only repeats the operation we cannot trust.
                    if let Err(retry_error) = redis_store::rescore_fanout_job(
                        &state.redis_pool,
                        &job_json,
                        state.settings.service.new_post_fanout_retry_secs,
                    )
                    .await
                    {
                        error!(error = %retry_error, "Failed to reschedule fan-out page after processing error; lease expiry will recover it");
                    } else {
                        crate::metrics::new_post_fanout_retry("worker_error", "preserved");
                    }
                }
            }
            Ok(None) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(poll_interval) => {}
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to claim durable new-post fan-out job");
                tokio::select! {
                    biased;
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(poll_interval) => {}
                }
            }
        }
    }

    info!("Durable new-post fan-out worker shut down.");
    Ok(())
}

async fn process_fanout_job(
    state: &AppState,
    job_json: &str,
    token: CancellationToken,
) -> Result<()> {
    let job: NewPostFanoutJob = match serde_json::from_str(job_json) {
        Ok(job) => job,
        Err(e) => {
            error!(error = %e, "Discarding malformed durable new-post fan-out job");
            redis_store::complete_fanout_job(&state.redis_pool, job_json, None).await?;
            return Ok(());
        }
    };
    let event: Event = match serde_json::from_str::<Event>(&job.event_json) {
        Ok(event) if event.kind.as_u16() == KIND_VIDEO => event,
        Ok(event) => {
            error!(event_id = %event.id, kind = %event.kind, "Discarding non-video fan-out job");
            redis_store::complete_fanout_job(&state.redis_pool, job_json, None).await?;
            return Ok(());
        }
        Err(e) => {
            error!(error = %e, "Discarding fan-out job with malformed event");
            redis_store::complete_fanout_job(&state.redis_pool, job_json, None).await?;
            return Ok(());
        }
    };

    if fanout_job_expired(&job, Timestamp::now().as_secs()) {
        warn!(event_id = %event.id, cursor = job.cursor, attempts = job.attempt, "Discarding expired durable fan-out page");
        redis_store::complete_fanout_job(&state.redis_pool, job_json, None).await?;
        crate::metrics::new_post_fanout_retry("lifetime", "expired");
        return Ok(());
    }

    let page = match redis_store::get_notify_watchers_page(
        &state.redis_pool,
        &event.pubkey,
        job.cursor,
        state.settings.service.new_post_fanout_page_size,
    )
    .await
    {
        Ok(page) => page,
        Err(e) => {
            error!(event_id = %event.id, cursor = job.cursor, error = %e, "Failed to read durable watcher page; scheduling retry");
            schedule_fanout_retry(state, job_json, &job, 0, "watcher_page_read", None).await?;
            return Ok(());
        }
    };

    let mentioned: HashSet<PublicKey> = video_mention_targets(&event)
        .into_iter()
        .map(|target| target.recipient)
        .collect();
    let targets = watcher_page_targets(page.watchers, &event.pubkey, &mentioned);
    let copy = match &job.sender_name {
        Some(sender_name) => LazyEventCopy::resolved(EventScopedCopy {
            sender_name: sender_name.clone(),
            formatted_content: None,
        }),
        None => LazyEventCopy::for_targets(&targets),
    };

    if !targets.is_empty() {
        info!(
            event_id = %event.id,
            recipient_count = targets.len(),
            cursor = job.cursor,
            next_cursor = page.next_cursor,
            "Processing durable new-post notification page"
        );
        if let Err(e) = send_notifications_bounded(
            state,
            &event,
            targets,
            &copy,
            token,
            state.settings.service.new_post_delivery_concurrency,
        )
        .await
        {
            if matches!(e, crate::error::ServiceError::Cancelled) {
                return Err(e);
            }
            let provider_delay = match &e {
                crate::error::ServiceError::RetryableDelivery(delay) => delay.as_secs(),
                _ => 0,
            };
            let exhausted_next_job = fanout_successor_job(&job, page.next_cursor, &copy)?;
            error!(event_id = %event.id, cursor = job.cursor, error = %e, "Durable watcher page was incomplete; scheduling retry");
            schedule_fanout_retry(
                state,
                job_json,
                &job,
                provider_delay,
                "delivery",
                exhausted_next_job.as_deref(),
            )
            .await?;
            return Ok(());
        }
    }

    let next_job = fanout_successor_job(&job, page.next_cursor, &copy)?;
    redis_store::complete_fanout_job(&state.redis_pool, job_json, next_job.as_deref()).await
}

fn fanout_successor_job(
    job: &NewPostFanoutJob,
    next_cursor: u64,
    copy: &LazyEventCopy,
) -> Result<Option<String>> {
    if next_cursor == 0 {
        return Ok(None);
    }

    let next = NewPostFanoutJob {
        event_json: job.event_json.clone(),
        cursor: next_cursor,
        sender_name: job
            .sender_name
            .clone()
            .or_else(|| copy.cell.get().map(|resolved| resolved.sender_name.clone())),
        attempt: 0,
        expires_at: job.expires_at,
    };
    Ok(Some(serde_json::to_string(&next)?))
}

async fn schedule_fanout_retry(
    state: &AppState,
    current_job: &str,
    job: &NewPostFanoutJob,
    minimum_delay_secs: u64,
    reason: &'static str,
    exhausted_next_job: Option<&str>,
) -> Result<()> {
    let Some((retry, delay)) = next_fanout_retry(
        job,
        state.settings.service.new_post_fanout_retry_secs,
        minimum_delay_secs,
        Timestamp::now().as_secs(),
    ) else {
        if let Some(successor) = exhausted_next_job {
            let delay = fanout_retry_delay(
                state.settings.service.new_post_fanout_retry_secs,
                MAX_FANOUT_ATTEMPTS,
                minimum_delay_secs,
            )
            .min(job.expires_at.saturating_sub(Timestamp::now().as_secs()));
            redis_store::retry_fanout_job(&state.redis_pool, current_job, successor, delay).await?;
        } else {
            redis_store::complete_fanout_job(&state.redis_pool, current_job, None).await?;
        }
        crate::metrics::new_post_fanout_retry(reason, "exhausted");
        error!(
            cursor = job.cursor,
            attempts = MAX_FANOUT_ATTEMPTS,
            reason,
            continued = exhausted_next_job.is_some(),
            "Discarding durable new-post fan-out page after its retry budget was exhausted"
        );
        return Ok(());
    };
    let retry_json = serde_json::to_string(&retry)?;
    redis_store::retry_fanout_job(&state.redis_pool, current_job, &retry_json, delay).await?;
    crate::metrics::new_post_fanout_retry(reason, "scheduled");
    Ok(())
}

fn next_fanout_retry(
    job: &NewPostFanoutJob,
    base_delay_secs: u64,
    minimum_delay_secs: u64,
    now: u64,
) -> Option<(NewPostFanoutJob, u64)> {
    // Twelve total attempts span about 30 minutes of default scheduled backoff
    // and 5-minute local cap. That rides out sustained provider backpressure
    // without letting one permanently bad page churn for the seven-day event TTL.
    if job.attempt.saturating_add(1) >= MAX_FANOUT_ATTEMPTS {
        return None;
    }
    let mut retry = job.clone();
    retry.attempt = retry.attempt.saturating_add(1);
    let delay = fanout_retry_delay(base_delay_secs, retry.attempt, minimum_delay_secs)
        .min(job.expires_at.saturating_sub(now));
    Some((retry, delay))
}

fn fanout_retry_delay(base_secs: u64, attempt: u16, minimum_delay_secs: u64) -> u64 {
    const MAX_RETRY_DELAY_SECS: u64 = 300;
    let exponent = u32::from(attempt.saturating_sub(1).min(6));
    base_secs
        .saturating_mul(1u64 << exponent)
        .min(MAX_RETRY_DELAY_SECS)
        // The cap applies to our exponential backoff, not the provider's explicit
        // floor. Retrying before FCM's Retry-After would amplify backpressure.
        .max(minimum_delay_secs)
}

fn fanout_job_expired(job: &NewPostFanoutJob, now: u64) -> bool {
    now >= job.expires_at
}

/// Find recipients for a reaction event (kind 7)
/// Returns the author of the event being reacted to
fn find_reaction_recipients(event: &Event) -> Vec<PublicKey> {
    // Look for 'e' tag pointing to the event being reacted to
    // and 'p' tag pointing to the author of that event
    event
        .tags
        .iter()
        .filter(|t| t.kind() == TagKind::p())
        .filter_map(|t| t.content())
        .filter_map(|content| PublicKey::from_str(content).ok())
        .collect()
}

/// Find recipients for a text note event (kind 1)
/// Could be a comment (e-tag) or mention (p-tag)
fn find_text_note_recipients(event: &Event) -> Vec<PublicKey> {
    // Get all p-tagged users (mentions or reply targets)
    find_mentioned_pubkeys(event)
}

/// Find recipients for a repost event (kind 16)
/// Returns the author of the reposted event
fn find_repost_recipients(event: &Event) -> Vec<PublicKey> {
    // Look for 'p' tag pointing to the author of the reposted event
    event
        .tags
        .iter()
        .filter(|t| t.kind() == TagKind::p())
        .filter_map(|t| t.content())
        .filter_map(|content| PublicKey::from_str(content).ok())
        .collect()
}

/// Extract all mentioned pubkeys from p-tags
fn find_mentioned_pubkeys(event: &Event) -> Vec<PublicKey> {
    event
        .tags
        .iter()
        .filter(|t| t.kind() == TagKind::p())
        .filter_map(|t| t.content())
        .filter_map(|content| PublicKey::from_str(content).ok())
        .collect()
}

/// One notification to deliver: who gets it, and what kind it is.
///
/// A single event can now yield more than one notification *type* — a video both
/// mentions someone and reaches the author's bell subscribers — so recipients
/// carry their own type rather than sharing one for the whole event.
#[derive(Debug, Clone, Copy)]
struct NotificationTarget {
    recipient: PublicKey,
    notification_type: NotificationType,
}

/// Drop targets that are the event's own author.
///
/// An event that `p`-tags its own sender — a self-reaction, a self-repost —
/// resolves to the author as a recipient, and nobody should be notified about
/// their own activity. Kept as a pure function, like `watcher_page_targets`,
/// so the rule is testable without an `AppState`.
fn deliverable_targets(
    targets: Vec<NotificationTarget>,
    author: &PublicKey,
) -> Vec<NotificationTarget> {
    targets
        .into_iter()
        .filter(|target| target.recipient != *author)
        .collect()
}

/// Give every recipient the same notification type.
fn targets_of(
    notification_type: NotificationType,
    recipients: Vec<PublicKey>,
) -> Vec<NotificationTarget> {
    recipients
        .into_iter()
        .map(|recipient| NotificationTarget {
            recipient,
            notification_type,
        })
        .collect()
}

/// The parts of a push payload that depend on the event rather than the recipient.
///
/// Resolved once per event and shared across the fan-out. Both fields used to be
/// computed inside `create_fcm_payload`, which runs per recipient, so both cost
/// scaled with the recipient count while their inputs did not.
///
/// That was affordable when an event reached one to three `p`-tagged recipients.
/// A bell reaches every watcher of the creator, and `get_display_name` is not a
/// cheap repeat: on a profile-cache miss it does a relay `fetch_events` with a
/// five-second timeout, and misses are never cached — only profiles that were
/// found are written back. So a creator with no kind-0 metadata paid one relay
/// round-trip per watcher, serially, on the single event-handler loop, delaying
/// every other user's notifications behind it.
struct EventScopedCopy {
    /// Display name of the event author, or a short npub when unresolvable.
    sender_name: String,
    /// Event content with mentions resolved, when any target renders it.
    ///
    /// `None` means "not resolved" — either no target needed it or the parse
    /// failed — and callers fall back to the raw content, as they did before.
    formatted_content: Option<String>,
}

/// Whether this notification type renders the event's content in its body.
///
/// Exhaustive rather than a `matches!` so a new notification type has to state
/// its answer here instead of silently defaulting to "no" and reaching
/// `create_fcm_payload` without the content it needs.
fn renders_event_content(notification_type: NotificationType) -> bool {
    match notification_type {
        NotificationType::Comment | NotificationType::Mention => true,
        NotificationType::Like | NotificationType::Repost | NotificationType::NewPost => false,
    }
}

/// The event-scoped copy, resolved at most once and only if it is needed.
///
/// Resolving once per event rather than once per recipient is the point of the
/// `EventScopedCopy` split. Resolving it *eagerly* is not: every gate in
/// `send_notification_to_user` — allowlist, registered tokens, preferences,
/// coordinate record, rate limit — can drop a recipient, and on a public relay
/// the token gate alone drops nearly all of them, because the subscription has
/// no `#p` narrowing and most tagged users have never registered for push.
///
/// An eager resolve therefore pays `get_display_name` for events that deliver
/// nothing. That is a Redis GET plus, on a cache miss, a relay `fetch_events`
/// bounded by the five seconds hard-coded in `MentionParser::fetch_from_relays`
/// — not by `query_timeout_secs`, which is declared in config and read nowhere,
/// so it is not a knob an operator can turn on this path. Misses are never
/// written back either, so an author with no kind-0 pays it on every event
/// forever. All of it on the single sequential handler task, ahead of every
/// other user's notifications.
///
/// Deferring the resolve to the first recipient that actually reaches payload
/// construction keeps the once-per-event property and restores the property the
/// per-recipient version had for free: an event nobody can be pushed for costs
/// no profile work at all.
///
/// What it gives up: the cell caches whatever `resolve_event_scoped_copy`
/// returns, and that includes the short-npub fallback it returns when the
/// profile lookup fails. On `main` the lookup sat inside `create_fcm_payload`,
/// so one recipient's timeout did not spoil the next one's — each retried.
/// Here the first recipient to resolve fixes the sender name for the whole
/// fan-out, so a single timed-out lookup shows every remaining watcher a short
/// npub instead of the creator's name.
///
/// That is the trade rather than an oversight. Retrying per recipient is what
/// made one unreachable profile relay cost five seconds *per recipient*, and a
/// bell fan-out is precisely where that multiplies. A degraded name on one
/// event is cheaper than stalling the handler for every other user's
/// notifications.
struct LazyEventCopy {
    cell: tokio::sync::OnceCell<EventScopedCopy>,
    /// Whether any target renders the event body, decided over the whole target
    /// list rather than the one recipient that happens to resolve it first.
    needs_content: bool,
}

impl LazyEventCopy {
    fn for_targets(targets: &[NotificationTarget]) -> Self {
        Self {
            cell: tokio::sync::OnceCell::new(),
            needs_content: targets
                .iter()
                .any(|target| renders_event_content(target.notification_type)),
        }
    }

    /// Pre-resolved copy for durable pages and focused delivery tests.
    fn resolved(copy: EventScopedCopy) -> Self {
        Self {
            cell: tokio::sync::OnceCell::new_with(Some(copy)),
            needs_content: false,
        }
    }

    async fn get(&self, state: &AppState, event: &Event) -> &EventScopedCopy {
        self.cell
            .get_or_init(|| resolve_event_scoped_copy(state, event, self.needs_content))
            .await
    }
}

/// Resolve the recipient-independent copy for an event, once.
///
/// The content parse is skipped entirely unless some target actually renders the
/// body, so a pure bell fan-out does not pay for mention parsing it never uses.
async fn resolve_event_scoped_copy(
    state: &AppState,
    event: &Event,
    needs_content: bool,
) -> EventScopedCopy {
    let Some(mention_parser) = state.mention_parser_service.as_ref() else {
        return EventScopedCopy {
            sender_name: format_short_npub(&event.pubkey),
            formatted_content: None,
        };
    };

    let sender_name = match mention_parser
        .get_display_name(&event.pubkey.to_hex())
        .await
    {
        Ok(Some(name)) => name,
        Ok(None) => format_short_npub(&event.pubkey),
        Err(e) => {
            warn!(error = %e, "Failed to get sender display name");
            format_short_npub(&event.pubkey)
        }
    };

    let formatted_content = if needs_content {
        match mention_parser.format_content_for_push(&event.content).await {
            Ok(formatted) => Some(formatted),
            Err(e) => {
                warn!(event_id = %event.id, error = %e, "Failed to format content for push");
                None
            }
        }
    } else {
        None
    };

    EventScopedCopy {
        sender_name,
        formatted_content,
    }
}

/// User-visible copy for a new-post push.
///
/// The body is required by the mobile client: an empty value makes foreground
/// handling return early without displaying a notification.
fn new_post_copy(sender_name: &str) -> (String, String) {
    (
        "New vine".to_string(),
        format!("{} posted a new vine", sender_name),
    )
}

/// Mention targets for a video event: p-tagged users other than the author.
fn video_mention_targets(event: &Event) -> Vec<NotificationTarget> {
    find_mentioned_pubkeys(event)
        .into_iter()
        .filter(|recipient| *recipient != event.pubkey)
        .map(|recipient| NotificationTarget {
            recipient,
            notification_type: NotificationType::Mention,
        })
        .collect()
}

/// Turn one page of bell watchers into new-post targets.
///
/// Mention wins on overlap: someone who both watches `author` and is mentioned
/// in the video gets exactly one push, typed `Mention`, because that is the more
/// specific signal. The mention pass runs first and hands its recipients here as
/// `mentioned`, so watchers it already covered are dropped rather than belled.
///
/// Kept separate from the Redis read so the rule is testable on its own.
fn watcher_page_targets(
    watchers: Vec<PublicKey>,
    author: &PublicKey,
    mentioned: &HashSet<PublicKey>,
) -> Vec<NotificationTarget> {
    watchers
        .into_iter()
        .filter(|watcher| watcher != author)
        .filter(|watcher| !mentioned.contains(watcher))
        .map(|recipient| NotificationTarget {
            recipient,
            notification_type: NotificationType::NewPost,
        })
        .collect()
}

/// Build the coordinate-and-recipient key used to deduplicate video edits.
///
/// Scoped by notification type as well as coordinate, because the two are
/// orthogonal: an edit does not change which *kind* of notification a recipient
/// is owed, so keying on type keeps the stable-across-edits property while
/// stopping one type from consuming another's record.
///
/// Without the type, a watcher who got a NewPost push for a video and was then
/// `p`-tagged in an edit of that same video had the mention suppressed for
/// `video_coordinate_dedup_ttl_secs` — a year by default. Belling a creator
/// quietly cost you mention notifications from them. `watcher_page_targets`
/// already makes mention win over bell within a single event; this extends the
/// same rule across edits, where the two pushes carry genuinely different
/// information ("X posted a vine" versus "X mentioned you").
fn video_recipient_claim_key(
    event: &Event,
    recipient: &PublicKey,
    notification_type: NotificationType,
) -> Option<String> {
    let d_tag = event.tags.identifier()?;
    Some(format!(
        "{}:{}:{}:{d_tag}:{}",
        event.kind.as_u16(),
        notification_type.display_name(),
        event.pubkey.to_hex(),
        recipient.to_hex()
    ))
}

/// Legacy video-coordinate key written before the claim was scoped by type.
///
/// Those keys live for `video_coordinate_dedup_ttl_secs`, one year by default,
/// so a deploy must keep reading them for one full TTL cycle. A legacy record
/// came from a video mention, which also satisfies a bell for the same
/// coordinate; see `satisfied_video_claims`.
fn legacy_video_recipient_claim_key(event: &Event, recipient: &PublicKey) -> Option<String> {
    let d_tag = event.tags.identifier()?;
    Some(format!(
        "{}:{}:{d_tag}:{}",
        event.kind.as_u16(),
        event.pubkey.to_hex(),
        recipient.to_hex()
    ))
}

/// The video-coordinate records a delivered push satisfies.
///
/// Type-scoping the claim key made the two directions independent, but they are
/// not: a mention push names the video, so it necessarily tells the recipient
/// that video exists — which is the entire content of a bell. Delivering one
/// therefore satisfies the bell's record too. The converse does not hold, since
/// "X posted a vine" says nothing about being mentioned, and that asymmetry is
/// why the key carries the type at all.
///
/// This is `watcher_page_targets`'s "mention wins on overlap" rule extended
/// across edits. Without it, a watcher who was `p`-tagged in the original and
/// dropped from an edit resolves to a bare `NewPost` target on the edit and is
/// told "posted a new vine" about a video they were already pushed about.
///
/// The guarantee is per-replica, not global. An event and its edit carry
/// different ids, so `try_claim_event` does not serialise them, and production
/// runs two replicas: two handlers reaching the coordinate check in the same
/// moment both read it absent and both send. What makes it hold on one replica
/// is that `run` awaits `route_event` inline, so the original's record is
/// written before the edit is read. Closing the cross-replica case means
/// `SET NX EX` on the claim, which costs the same thing the rate-limit comment
/// in `send_notification_to_user` declines to pay: a failed send would burn the
/// record and that recipient would never get the push at all.
///
/// Pure, so the rule is testable without Redis.
fn satisfied_video_claims(notification_type: NotificationType) -> Vec<NotificationType> {
    match notification_type {
        NotificationType::Mention => vec![NotificationType::Mention, NotificationType::NewPost],
        other => vec![other],
    }
}

async fn has_video_claim(
    state: &AppState,
    event: &Event,
    target_pubkey: &PublicKey,
    notification_type: NotificationType,
    token: &CancellationToken,
) -> Result<bool> {
    let Some(claim_key) = video_recipient_claim_key(event, target_pubkey, notification_type) else {
        warn!(
            event_id = %event.id,
            target_pubkey = %target_pubkey,
            "Skipping video notification without an addressable d-tag"
        );
        return Ok(true);
    };
    let redis_key = format!("dedup:{claim_key}");
    let already_notified = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event.id, target_pubkey = %target_pubkey, "Cancelled while checking video recipient delivery.");
            return Err(crate::error::ServiceError::Cancelled);
        }
        lookup_result = redis_store::get_cached_string(&state.redis_pool, &redis_key) => {
            lookup_result?.is_some()
        }
    };
    if already_notified {
        return Ok(true);
    }

    let Some(legacy_claim_key) = legacy_video_recipient_claim_key(event, target_pubkey) else {
        return Ok(false);
    };
    let legacy_redis_key = format!("dedup:{legacy_claim_key}");
    tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event.id, target_pubkey = %target_pubkey, "Cancelled while checking legacy video recipient delivery.");
            Err(crate::error::ServiceError::Cancelled)
        }
        lookup_result = redis_store::get_cached_string(&state.redis_pool, &legacy_redis_key) => {
            Ok(lookup_result?.is_some())
        }
    }
}

/// Record the video-coordinate claims a decision about `target_pubkey` settles.
///
/// Infallible on purpose. Every caller reaches this after the delivery decision
/// is final — the push has shipped, or the rate limit has deliberately dropped
/// it — so a failed write here is bookkeeping loss, not a reason to abandon the
/// work that follows. `a10a02b`'s `return` in the old inline loop skipped
/// invalid-token removal for the same reason `726bd1e` had to make it a
/// `break`; returning `()` keeps that class of bug from coming back through a
/// `?` at a call site.
async fn record_video_claims(
    state: &AppState,
    event: &Event,
    target_pubkey: &PublicKey,
    notification_type: NotificationType,
    log_message: &'static str,
) {
    if event.kind.as_u16() != KIND_VIDEO {
        return;
    }

    for satisfied in satisfied_video_claims(notification_type) {
        let Some(claim_key) = video_recipient_claim_key(event, target_pubkey, satisfied) else {
            warn!(
                event_id = %event.id,
                target_pubkey = %target_pubkey,
                "Video notification lacked an addressable d-tag"
            );
            return;
        };
        let redis_key = format!("dedup:{claim_key}");
        if let Err(e) = redis_store::set_cached_string(
            &state.redis_pool,
            &redis_key,
            "1",
            state.settings.service.video_coordinate_dedup_ttl_secs,
        )
        .await
        {
            error!(
                event_id = %event.id,
                target_pubkey = %target_pubkey,
                claim = %redis_key,
                error = %e,
                message = log_message,
                "Failed to record a video-coordinate claim"
            );
        }
    }
}

/// Find recipients for a NIP-22 comment event (kind 1111).
///
/// Per NIP-22 the uppercase `P` tag is the root-scope author (for a video
/// comment, the video owner) and the lowercase `p` tag is the parent-item
/// author (for a reply, the parent comment's author — *not* the owner). Both
/// are notified so the owner hears about comments on their video and the
/// replied-to author hears about the reply; the two coincide for a top-level
/// comment, so the result is deduplicated. The authoritative routing target
/// (the root video) is attached separately by `create_fcm_payload` from the
/// uppercase `A`/`E` root scope.
fn find_comment_recipients(event: &Event) -> Vec<PublicKey> {
    let root_author = TagKind::single_letter(Alphabet::P, true);
    let parent_author = TagKind::p();

    let mut recipients: Vec<PublicKey> = Vec::new();
    for tag in event.tags.iter() {
        let tag_kind = tag.kind();
        if tag_kind != root_author && tag_kind != parent_author {
            continue;
        }
        if let Some(pubkey) = tag.content().and_then(|c| PublicKey::from_str(c).ok()) {
            if !recipients.contains(&pubkey) {
                recipients.push(pubkey);
            }
        }
    }
    recipients
}

/// What one FCM send result means for the delivery summary.
#[derive(Debug, PartialEq, Eq)]
enum SendOutcome {
    Delivered,
    /// Not delivered *and* the token should be pruned. It is both — counting it
    /// only as a prune is what let a total delivery failure report zero
    /// failures.
    FailedAndPrune,
    Failed,
}

fn classify_send_outcome(result: &std::result::Result<(), fcm_sender::FcmError>) -> SendOutcome {
    match result {
        Ok(()) => SendOutcome::Delivered,
        Err(fcm_sender::FcmError::TokenNotRegistered) => SendOutcome::FailedAndPrune,
        Err(_) => SendOutcome::Failed,
    }
}

/// Send a notification to a specific user
#[instrument(skip_all, fields(target_pubkey = %target_pubkey.to_string(), notification_type = ?notification_type))]
async fn send_notification_to_user(
    state: &AppState,
    event: &Event,
    target_pubkey: &PublicKey,
    notification_type: NotificationType,
    copy: &LazyEventCopy,
    token: CancellationToken,
) -> Result<()> {
    let event_id = event.id;
    let pubkey_hex = target_pubkey.to_hex();

    // Check pubkey allowlist (for non-production environments)
    let allowed = &state.settings.service.allowed_pubkeys;
    if !allowed.is_empty() && !allowed.contains(&pubkey_hex) {
        debug!(
            event_id = %event_id,
            target_pubkey = %pubkey_hex,
            "Skipping notification - pubkey not in allowed list"
        );
        return Ok(());
    }

    // Check if user has tokens registered
    let tokens = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while fetching tokens.");
            return Err(crate::error::ServiceError::Cancelled);
        }
        res = redis_store::get_tokens_for_pubkey(&state.redis_pool, target_pubkey) => {
            res?
        }
    };

    if tokens.is_empty() {
        info!(
            event_id = %event_id,
            target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
            "No FCM tokens registered for recipient - skipping notification"
        );
        return Ok(());
    }

    // Check user preferences
    let prefs = preferences::get_user_preferences(
        &state.redis_pool,
        &pubkey_hex,
        &state.settings.notification.default_preferences,
    )
    .await?;

    let mut delivery_type = notification_type;
    if !delivery_type.is_enabled(&prefs) {
        let can_fall_back_to_bell = if event.kind.as_u16() == KIND_VIDEO
            && delivery_type == NotificationType::Mention
            && NotificationType::NewPost.is_enabled(&prefs)
        {
            match redis_store::is_notify_watcher(&state.redis_pool, &event.pubkey, target_pubkey)
                .await
            {
                Ok(is_watcher) => is_watcher,
                Err(e) => {
                    error!(
                        event_id = %event_id,
                        target_pubkey = %target_pubkey,
                        creator = %event.pubkey,
                        error = %e,
                        "Failed to check bell fallback for muted video mention"
                    );
                    false
                }
            }
        } else {
            false
        };

        if can_fall_back_to_bell {
            delivery_type = NotificationType::NewPost;
            info!(
                event_id = %event_id,
                target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
                "Video mention disabled but bell enabled for watcher - delivering new-post notification"
            );
        } else {
            info!(
                event_id = %event_id,
                target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
                notification_type = ?delivery_type,
                "Notification type disabled by user preferences - skipping"
            );
            return Ok(());
        }
    }

    if event.kind.as_u16() == KIND_VIDEO
        && has_video_claim(state, event, target_pubkey, delivery_type, &token).await?
    {
        trace!(
            event_id = %event_id,
            target_pubkey = %target_pubkey,
            "Skipping video recipient already notified for this coordinate"
        );
        return Ok(());
    }

    if delivery_type == NotificationType::NewPost {
        let rate_key = redis_store::build_notify_rate_key(target_pubkey, &event.pubkey);
        let within_window = tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while checking the new-post rate limit.");
                return Err(crate::error::ServiceError::Cancelled);
            }
            lookup_result = redis_store::get_cached_string(&state.redis_pool, &rate_key) => {
                lookup_result?.is_some()
            }
        };

        if within_window {
            info!(
                event_id = %event_id,
                target_pubkey = %target_pubkey,
                creator = %event.pubkey,
                "Skipping new-post notification inside the per-creator rate-limit window"
            );
            record_video_claims(
                state,
                event,
                target_pubkey,
                delivery_type,
                "Failed to record a rate-limited video-coordinate claim; an edit may re-notify",
            )
            .await;
            return Ok(());
        }
    }

    let recipient_claim = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled before claiming recipient delivery.");
            return Err(crate::error::ServiceError::Cancelled);
        }
        claim_result = redis_store::try_claim_recipient_event(
            &state.redis_pool,
            &event_id,
            target_pubkey,
            state.settings.service.processed_event_ttl_secs,
        ) => claim_result?
    };
    let Some(recipient_claim) = recipient_claim else {
        trace!(
            event_id = %event_id,
            target_pubkey = %target_pubkey,
            "Skipping recipient already claimed for this event"
        );
        return Ok(());
    };

    info!(
        event_id = %event_id,
        target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
        token_count = tokens.len(),
        "Found FCM tokens for recipient"
    );

    // Every gate that could drop this recipient is behind us, so this delivery
    // is the one that pays for the event's copy — once, for all recipients.
    let copy = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while resolving the event copy.");
            if let Err(e) = redis_store::release_recipient_event_claim(
                &state.redis_pool,
                &recipient_claim,
            ).await {
                error!(event_id = %event_id, target_pubkey = %target_pubkey, error = %e, "Failed to release unstarted recipient claim during cancellation");
            }
            return Err(crate::error::ServiceError::Cancelled);
        }
        resolved = copy.get(state, event) => resolved
    };

    // Create FCM payload
    let payload = create_fcm_payload(event, target_pubkey, delivery_type, copy);

    // Send to all tokens
    info!(
        event_id = %event_id,
        target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
        token_count = tokens.len(),
        "Sending FCM notification"
    );

    let results = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled during FCM send.");
            return Err(crate::error::ServiceError::Cancelled);
        }
        send_result = state.fcm_client.send_batch(&tokens, payload) => {
            send_result
        }
    };

    // Process results
    let mut tokens_to_remove = Vec::new();
    // Doubles as the success count. A delivered push is the only evidence this
    // service gets that a device still exists, so the tokens are worth keeping
    // rather than just counting.
    let mut delivered_tokens = Vec::new();
    // Counted separately from `tokens_to_remove`: a dead token is a *pruned*
    // token, not the only way a send fails. Reporting only removals meant an
    // outage where every send failed for some other reason — bad credentials,
    // FCM 5xx, a timeout — still summarised as `failed_count=0`, which reads as
    // "nothing went wrong" in exactly the logs an operator checks first.
    let mut failed_count = 0;
    let mut retryable_failure = None;

    for (fcm_token, result) in results {
        if token.is_cancelled() {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while processing FCM results.");
            return Err(crate::error::ServiceError::Cancelled);
        }

        let truncated_token = fcm_sender::token_prefix(&fcm_token);

        match classify_send_outcome(&result) {
            SendOutcome::Delivered => {
                trace!(target_pubkey = %target_pubkey, token_prefix = %truncated_token, "Successfully sent notification");
                delivered_tokens.push(fcm_token);
            }
            SendOutcome::FailedAndPrune => {
                failed_count += 1;
                warn!(target_pubkey = %target_pubkey, token_prefix = %truncated_token, "Token invalid/unregistered, marking for removal.");
                tokens_to_remove.push(fcm_token);
            }
            SendOutcome::Failed => {
                failed_count += 1;
                // Display, not Debug: `?result.as_ref().err()` renders
                // `Some(Unauthorized("..."))`, wrapping the one thing an
                // operator reads this line for in two layers of noise.
                let reason = result
                    .as_ref()
                    .err()
                    .map_or_else(String::new, ToString::to_string);
                error!(
                    target_pubkey = %target_pubkey,
                    token_prefix = %truncated_token,
                    error = %reason,
                    "FCM send failed for token"
                );
                if let Err(fcm_sender::FcmError::RetryableInternal(delay)) = &result {
                    retryable_failure = Some(*delay);
                }
            }
        }
    }

    let success_count = delivered_tokens.len();

    info!(
        event_id = %event_id,
        target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
        success_count,
        failed_count,
        pruned_count = tokens_to_remove.len(),
        "FCM notification send summary"
    );

    // A delivered push is proof the device is still there, so it has to move the
    // token away from the staleness sweep. Without this the score is only ever
    // written at registration, and `cleanup_stale_tokens` deletes devices that
    // are actively receiving notifications but have not re-registered inside the
    // window — silently, with no error anywhere.
    //
    // Log and continue rather than `?`, for the same reason as the bookkeeping
    // below: the push has already shipped, and propagating here would report a
    // delivered notification as failed and skip the writes that follow.
    if !delivered_tokens.is_empty() {
        if let Err(e) =
            redis_store::refresh_token_activity(&state.redis_pool, &delivered_tokens).await
        {
            error!(
                event_id = %event_id,
                target_pubkey = %target_pubkey,
                error = %e,
                "Failed to refresh token activity after a delivered push; the sweep may \
                 deregister a live device"
            );
        }
    }

    // Open the rate-limit window only on a delivered push.
    //
    // This is check-then-set-on-success rather than an atomic `SET NX EX`, both
    // to mirror the video-coordinate dedup above and — more importantly — so a
    // failed FCM send does not burn the user's hour-long window. The tradeoff is
    // that two replicas handling different videos from the same creator in the
    // same instant can both pass the check and double-send. That race is rare,
    // bounded, and low-harm; silently eating an hour of notifications on an FCM
    // blip is worse. Do not "fix" this into `SET NX EX`.
    if delivery_type == NotificationType::NewPost && success_count > 0 {
        let rate_key = redis_store::build_notify_rate_key(target_pubkey, &event.pubkey);
        // Log and continue rather than `?`. Everything from here down is
        // bookkeeping about a push that has already shipped, so propagating
        // reports a delivered notification as failed and, worse, skips the
        // bookkeeping below it: the coordinate claim that stops a NIP-33 edit
        // re-notifying, and invalid-token removal.
        if let Err(e) = redis_store::set_cached_string(
            &state.redis_pool,
            &rate_key,
            "1",
            state.settings.service.new_post_rate_limit_secs,
        )
        .await
        {
            error!(
                event_id = %event_id,
                target_pubkey = %target_pubkey,
                error = %e,
                "Failed to open the new-post rate-limit window after a delivered push"
            );
        }
    }

    if success_count > 0 {
        // Same reasoning as the rate-limit write above, and it matters more
        // here: `satisfied_video_claims` can yield two records, and `?` on
        // the first left the second unwritten. That is exactly the
        // half-written state the type-scoped claim exists to prevent.
        record_video_claims(
            state,
            event,
            target_pubkey,
            delivery_type,
            "Failed to record a video-coordinate claim after a delivered push; an edit may re-notify",
        )
        .await;
    }

    // Remove invalid tokens
    if !tokens_to_remove.is_empty() {
        debug!(event_id = %event_id, target_pubkey = %target_pubkey, count = tokens_to_remove.len(), "Removing invalid tokens");
        for fcm_token_to_remove in tokens_to_remove {
            if token.is_cancelled() {
                info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while removing invalid tokens.");
                return Err(crate::error::ServiceError::Cancelled);
            }
            let truncated_token = fcm_sender::token_prefix(&fcm_token_to_remove);
            match redis_store::remove_token(&state.redis_pool, target_pubkey, &fcm_token_to_remove)
                .await
            {
                Ok(removed) => {
                    record_invalid_token_pruned(removed);
                    if removed {
                        info!(target_pubkey = %target_pubkey, token_prefix = %truncated_token, "Removed invalid token");
                    } else {
                        debug!(target_pubkey = %target_pubkey, token_prefix = %truncated_token, "Invalid token was already removed");
                    }
                }
                Err(e) => {
                    error!(
                        target_pubkey = %target_pubkey, token_prefix = %truncated_token, error = %e,
                        "Failed to remove invalid token"
                    );
                }
            }
        }
    }

    // The claim is per recipient, while FCM reports per token. Once any device
    // receives the notification, retaining the recipient claim avoids resending
    // to that successful device merely to retry a sibling token. Only an
    // all-token retryable failure is safe to release and replay.
    if success_count == 0 {
        if let Some(delay) = retryable_failure {
            redis_store::release_recipient_event_claim(&state.redis_pool, &recipient_claim).await?;
            return Err(crate::error::ServiceError::RetryableDelivery(delay));
        }
    }

    Ok(())
}

fn record_invalid_token_pruned(removed: bool) {
    if removed {
        crate::metrics::tokens_pruned("invalid", 1);
    }
}

/// Create FCM payload for a notification
///
/// Takes the event-scoped copy rather than resolving it, so the relay and Redis
/// work behind `sender_name` and `formatted_content` happens once per event
/// instead of once per recipient. That leaves this function free of I/O, which
/// is also what makes it directly testable.
fn create_fcm_payload(
    event: &Event,
    target_pubkey: &PublicKey,
    notification_type: NotificationType,
    copy: &EventScopedCopy,
) -> FcmPayload {
    let mut data = std::collections::HashMap::new();

    let sender_name = copy.sender_name.clone();

    // Falls back to the raw content when the parse was skipped or failed, which
    // is what the per-recipient parse did on error.
    let formatted_content = || {
        copy.formatted_content
            .clone()
            .unwrap_or_else(|| event.content.clone())
    };

    // Generate title and body based on notification type
    let (title, body) = match notification_type {
        NotificationType::Like => {
            let title = "New like".to_string();
            let body = format!("{} liked your post", sender_name);
            (title, body)
        }
        NotificationType::Comment => {
            let title = "New comment".to_string();
            let body = format!(
                "{}: {}",
                sender_name,
                truncate_string(&formatted_content(), 150)
            );
            (title, body)
        }
        NotificationType::Mention => {
            let title = "You were mentioned".to_string();
            let body = format!(
                "{}: {}",
                sender_name,
                truncate_string(&formatted_content(), 150)
            );
            (title, body)
        }
        NotificationType::Repost => {
            let title = "New repost".to_string();
            let body = format!("{} reposted your post", sender_name);
            (title, body)
        }
        NotificationType::NewPost => {
            // Provisional copy. divine-mobile/brand-guidelines/TONE_OF_VOICE.md
            // governs user-facing strings; confirm before release.
            new_post_copy(&sender_name)
        }
    };

    // Build data payload
    data.insert(
        "type".to_string(),
        notification_type.display_name().to_string(),
    );
    data.insert("eventId".to_string(), event.id.to_hex());
    data.insert("title".to_string(), title.clone());
    data.insert("body".to_string(), body.clone());
    data.insert("senderPubkey".to_string(), event.pubkey.to_hex());
    data.insert("senderName".to_string(), sender_name);
    data.insert("receiverPubkey".to_string(), target_pubkey.to_hex());
    data.insert(
        "receiverNpub".to_string(),
        target_pubkey.to_bech32().unwrap_or_default(),
    );
    data.insert("eventKind".to_string(), event.kind.as_u16().to_string());
    data.insert(
        "timestamp".to_string(),
        event.created_at.as_secs().to_string(),
    );

    // Add authoritative routing/attribution target fields: the referenced event
    // id and, for addressable targets (e.g. kind 34236 videos), the signed
    // coordinate (`referencedAddress` + components) so the client never has to
    // guess the target's owner.
    insert_trigger_reference_fields(&mut data, event);

    FcmPayload {
        notification: None, // Data-only message for better client control
        data: Some(data),
        android: None,
        webpush: None,
        apns: None,
    }
}

/// Authoritative addressable target extracted from an event's `A`/`a` tag.
///
/// `address` is the full NIP-01 coordinate (`kind:pubkey:d-tag`); the remaining
/// fields are its components. The owner pubkey comes from the coordinate signed
/// into the actor's event, so consumers never have to infer ownership.
struct ReferencedCoordinate {
    address: String,
    kind: String,
    author_pubkey: String,
    d_tag: String,
}

/// Insert routing/attribution target fields into the FCM data map.
///
/// Adds `referencedEventId` (root-aware: prefers the NIP-22 uppercase `E` root
/// scope, else the lowercase `e` tag) and, when the event references an
/// addressable event, the authoritative `referencedAddress` coordinate split
/// into `referencedKind`, `referencedAuthorPubkey`, and `referencedDTag`.
fn insert_reference_fields(data: &mut std::collections::HashMap<String, String>, event: &Event) {
    if let Some(event_id) = referenced_event_id(event) {
        data.insert("referencedEventId".to_string(), event_id);
    }
    if let Some(coord) = referenced_coordinate(event) {
        data.insert("referencedAddress".to_string(), coord.address);
        data.insert("referencedKind".to_string(), coord.kind);
        data.insert("referencedAuthorPubkey".to_string(), coord.author_pubkey);
        data.insert("referencedDTag".to_string(), coord.d_tag);
    }
}

/// Insert routing fields for either a direct video trigger or a reference.
fn insert_trigger_reference_fields(
    data: &mut std::collections::HashMap<String, String>,
    event: &Event,
) {
    if event.kind.as_u16() == KIND_VIDEO {
        insert_video_reference_fields(data, event);
    } else {
        insert_reference_fields(data, event);
    }
}

/// Insert routing fields derived from a video trigger's own identity.
fn insert_video_reference_fields(
    data: &mut std::collections::HashMap<String, String>,
    event: &Event,
) {
    data.insert("referencedEventId".to_string(), event.id.to_hex());

    let Some(d_tag) = event.tags.identifier() else {
        return;
    };
    let kind = event.kind.as_u16().to_string();
    let author_pubkey = event.pubkey.to_hex();
    let address = format!("{kind}:{author_pubkey}:{d_tag}");

    data.insert("referencedAddress".to_string(), address);
    data.insert("referencedKind".to_string(), kind);
    data.insert("referencedAuthorPubkey".to_string(), author_pubkey);
    data.insert("referencedDTag".to_string(), d_tag.to_string());
}

/// Root-aware referenced event id.
///
/// Prefers the NIP-22 uppercase `E` root scope when present (so a comment
/// anchors to the root video, not the parent comment), otherwise the lowercase
/// `e` tag used by reactions and reposts.
fn referenced_event_id(event: &Event) -> Option<String> {
    event
        .tags
        .find(TagKind::single_letter(Alphabet::E, true))
        .or_else(|| event.tags.find(TagKind::e()))
        .and_then(|tag| tag.content())
        .map(str::to_string)
}

/// Authoritative addressable target from the event's `A` (NIP-22 root) tag,
/// falling back to the lowercase `a` (parent) tag.
///
/// Returns `None` when there is no addressable reference, or when the
/// coordinate is not a well-formed `kind:pubkey:d-tag` (numeric kind, non-empty
/// pubkey and d-tag). A d-tag may itself contain `:`, so only the first two
/// separators are split.
fn referenced_coordinate(event: &Event) -> Option<ReferencedCoordinate> {
    let address = event
        .tags
        .find(TagKind::single_letter(Alphabet::A, true))
        .or_else(|| event.tags.find(TagKind::a()))
        .and_then(|tag| tag.content())?;

    let mut parts = address.splitn(3, ':');
    let kind = parts.next()?;
    let author_pubkey = parts.next()?;
    let d_tag = parts.next()?;

    if kind.parse::<u32>().is_err() || author_pubkey.is_empty() || d_tag.is_empty() {
        return None;
    }

    Some(ReferencedCoordinate {
        address: address.to_string(),
        kind: kind.to_string(),
        author_pubkey: author_pubkey.to_string(),
        d_tag: d_tag.to_string(),
    })
}

/// Format a short version of an npub for display
fn format_short_npub(pubkey: &PublicKey) -> String {
    pubkey
        .to_bech32()
        .map(|npub| {
            if npub.len() > 12 {
                format!("{}...", &npub[..12])
            } else {
                npub
            }
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Truncate a string to a maximum length
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max_len).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fcm_sender::{FcmClient, FcmError, MockFcmSender};
    use nostr_sdk::prelude::{Keys, SecretKey};
    use std::sync::OnceLock;

    fn fanout_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[test]
    fn invalid_pruning_records_confirmed_removal() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            record_invalid_token_pruned(true);
            record_invalid_token_pruned(false);
        });

        handle.run_upkeep();
        let rendered = handle.render();
        assert!(
            rendered.contains(r#"push_tokens_pruned_total{reason="invalid"} 1"#),
            "{rendered}"
        );
    }

    async fn test_redis_pool() -> Option<redis_store::RedisPool> {
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
        let pool = redis_store::create_pool(&redis_url, 5).await.ok()?;
        let mut conn = pool.get().await.ok()?;
        let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
        drop(conn);
        pong.ok().map(|_| pool)
    }

    fn test_app_state(
        settings: crate::config::Settings,
        redis_pool: redis_store::RedisPool,
        fcm_client: FcmClient,
    ) -> AppState {
        AppState {
            settings,
            redis_pool,
            fcm_client: Arc::new(fcm_client),
            service_keys: None,
            crypto_service: None,
            nostr_client: Arc::new(Client::default()),
            profile_client: Arc::new(Client::default()),
            mention_parser_service: None,
        }
    }

    /// Event-scoped copy for tests that exercise delivery rather than copy.
    fn test_copy() -> LazyEventCopy {
        LazyEventCopy::resolved(EventScopedCopy {
            sender_name: "tester".to_string(),
            formatted_content: None,
        })
    }

    fn test_event_id(seed: u64) -> EventId {
        let mut bytes = [0u8; 32];
        bytes[24..].copy_from_slice(&seed.to_be_bytes());
        EventId::from_slice(&bytes).expect("32 bytes is a valid event id")
    }

    fn encrypted_deregistration_event(user_keys: &Keys, service_keys: &Keys, token: &str) -> Event {
        let payload = serde_json::json!({ "token": token }).to_string();
        let encrypted = nostr_sdk::nips::nip44::encrypt(
            user_keys.secret_key(),
            &service_keys.public_key(),
            payload,
            nostr_sdk::nips::nip44::Version::V2,
        )
        .expect("test token should encrypt");

        EventBuilder::new(Kind::from(KIND_DEREGISTRATION), encrypted)
            .tag(Tag::public_key(service_keys.public_key()))
            .sign_with_keys(user_keys)
            .expect("test deregistration should sign")
    }

    #[tokio::test]
    async fn deregistration_keeps_the_account_preferences() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let user = Keys::generate();
        let service_keys = Keys::generate();
        let token = format!("deregistration-owner-{}", user.public_key().to_hex());
        let expected_preferences = preferences::UserPreferences { kinds: vec![7] };
        let settings = crate::config::Settings::new().unwrap();

        redis_store::add_or_update_token(&pool, &user.public_key(), &token)
            .await
            .unwrap();
        preferences::set_user_preferences(
            &pool,
            &user.public_key().to_hex(),
            &expected_preferences,
        )
        .await
        .unwrap();

        let mut state = test_app_state(
            settings.clone(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(MockFcmSender::new())),
        );
        state.service_keys = Some(service_keys.clone());
        state.crypto_service = Some(CryptoService::new(service_keys.clone()));
        let event = encrypted_deregistration_event(&user, &service_keys, &token);

        handle_deregistration(&state, &event).await.unwrap();

        assert!(
            redis_store::get_tokens_for_pubkey(&pool, &user.public_key())
                .await
                .unwrap()
                .is_empty(),
            "deregistration should remove the owned token"
        );
        let stored_preferences = preferences::get_user_preferences(
            &pool,
            &user.public_key().to_hex(),
            &settings.notification.default_preferences,
        )
        .await
        .unwrap();
        assert_eq!(stored_preferences, expected_preferences);

        preferences::delete_user_preferences(&pool, &user.public_key().to_hex())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_rejected_deregistration_keeps_both_the_token_and_the_preferences() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let first_user = Keys::generate();
        let current_owner = Keys::generate();
        let service_keys = Keys::generate();
        let token = format!(
            "deregistration-transfer-{}",
            first_user.public_key().to_hex()
        );
        let expected_preferences = preferences::UserPreferences { kinds: vec![16] };
        let settings = crate::config::Settings::new().unwrap();

        redis_store::add_or_update_token(&pool, &first_user.public_key(), &token)
            .await
            .unwrap();
        preferences::set_user_preferences(
            &pool,
            &first_user.public_key().to_hex(),
            &expected_preferences,
        )
        .await
        .unwrap();
        redis_store::add_or_update_token(&pool, &current_owner.public_key(), &token)
            .await
            .unwrap();

        let mut state = test_app_state(
            settings.clone(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(MockFcmSender::new())),
        );
        state.service_keys = Some(service_keys.clone());
        state.crypto_service = Some(CryptoService::new(service_keys.clone()));
        let event = encrypted_deregistration_event(&first_user, &service_keys, &token);

        handle_deregistration(&state, &event).await.unwrap();

        let owner_tokens = redis_store::get_tokens_for_pubkey(&pool, &current_owner.public_key())
            .await
            .unwrap();
        assert_eq!(owner_tokens, vec![token.clone()]);
        let stored_preferences = preferences::get_user_preferences(
            &pool,
            &first_user.public_key().to_hex(),
            &settings.notification.default_preferences,
        )
        .await
        .unwrap();
        assert_eq!(stored_preferences, expected_preferences);

        redis_store::remove_token(&pool, &current_owner.public_key(), &token)
            .await
            .unwrap();
        preferences::delete_user_preferences(&pool, &first_user.public_key().to_hex())
            .await
            .unwrap();
    }

    #[test]
    fn test_is_event_too_old() {
        // Create a recent event
        let keys = Keys::generate();
        let recent_event = EventBuilder::text_note("test")
            .sign_with_keys(&keys)
            .unwrap();
        assert!(!is_event_too_old(&recent_event));

        // Create an old event (would need to mock timestamp)
        // This is a basic smoke test
    }

    #[test]
    fn test_format_short_npub() {
        let sk =
            SecretKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let keys = Keys::new(sk);
        let short = format_short_npub(&keys.public_key());
        assert!(short.starts_with("npub"));
        assert!(short.ends_with("..."));
        assert!(short.len() <= 15); // "npub" + 8 chars + "..."
    }

    #[test]
    fn test_truncate_string() {
        assert_eq!(truncate_string("hello", 10), "hello");
        assert_eq!(truncate_string("hello world!", 5), "hello...");
        assert_eq!(truncate_string("", 5), "");
    }

    // =========================================================================
    // Recipient Finding Tests
    // =========================================================================

    #[test]
    fn test_find_reaction_recipients_with_p_tag() {
        let sender = Keys::generate();
        let target = Keys::generate();

        // Create a reaction event with p-tag pointing to target
        let event = EventBuilder::new(Kind::Reaction, "+")
            .tag(Tag::public_key(target.public_key()))
            .tag(Tag::event(EventId::all_zeros()))
            .sign_with_keys(&sender)
            .unwrap();

        let recipients = find_reaction_recipients(&event);
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0], target.public_key());
    }

    #[test]
    fn test_find_reaction_recipients_no_p_tag() {
        let sender = Keys::generate();

        // Create a reaction without p-tag (malformed)
        let event = EventBuilder::new(Kind::Reaction, "+")
            .tag(Tag::event(EventId::all_zeros()))
            .sign_with_keys(&sender)
            .unwrap();

        let recipients = find_reaction_recipients(&event);
        assert!(recipients.is_empty());
    }

    #[test]
    fn test_find_text_note_recipients_with_mentions() {
        let sender = Keys::generate();
        let mentioned1 = Keys::generate();
        let mentioned2 = Keys::generate();

        let event = EventBuilder::text_note("Hello @someone")
            .tag(Tag::public_key(mentioned1.public_key()))
            .tag(Tag::public_key(mentioned2.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        let recipients = find_text_note_recipients(&event);
        assert_eq!(recipients.len(), 2);
        assert!(recipients.contains(&mentioned1.public_key()));
        assert!(recipients.contains(&mentioned2.public_key()));
    }

    #[test]
    fn test_find_text_note_recipients_no_mentions() {
        let sender = Keys::generate();

        let event = EventBuilder::text_note("Just a regular post")
            .sign_with_keys(&sender)
            .unwrap();

        let recipients = find_text_note_recipients(&event);
        assert!(recipients.is_empty());
    }

    #[test]
    fn test_find_repost_recipients() {
        let sender = Keys::generate();
        let original_author = Keys::generate();

        // Kind 16 repost with p-tag to original author
        let event = EventBuilder::new(Kind::from(16), "")
            .tag(Tag::public_key(original_author.public_key()))
            .tag(Tag::event(EventId::all_zeros()))
            .sign_with_keys(&sender)
            .unwrap();

        let recipients = find_repost_recipients(&event);
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0], original_author.public_key());
    }

    #[test]
    fn test_find_mentioned_pubkeys_multiple() {
        let sender = Keys::generate();
        let user1 = Keys::generate();
        let user2 = Keys::generate();
        let user3 = Keys::generate();

        let event = EventBuilder::text_note("Mentioning several people")
            .tag(Tag::public_key(user1.public_key()))
            .tag(Tag::public_key(user2.public_key()))
            .tag(Tag::public_key(user3.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        let pubkeys = find_mentioned_pubkeys(&event);
        assert_eq!(pubkeys.len(), 3);
    }

    #[test]
    fn test_video_notification_resolves_mentions_with_mention_type() {
        let sender = Keys::generate();
        let mentioned1 = Keys::generate();
        let mentioned2 = Keys::generate();

        let event = EventBuilder::new(Kind::from(34236), "inspired video")
            .tag(Tag::identifier("video-id"))
            .tag(Tag::public_key(mentioned1.public_key()))
            .tag(Tag::public_key(mentioned2.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        let targets = video_mention_targets(&event);

        assert_eq!(targets.len(), 2);
        assert!(targets
            .iter()
            .all(|t| t.notification_type == NotificationType::Mention));
        assert_eq!(NotificationType::Mention.display_name(), "mention");
        let recipients: Vec<PublicKey> = targets.iter().map(|t| t.recipient).collect();
        assert!(recipients.contains(&mentioned1.public_key()));
        assert!(recipients.contains(&mentioned2.public_key()));
    }

    #[test]
    fn test_video_notification_skips_self_reference() {
        let sender = Keys::generate();
        let event = EventBuilder::new(Kind::from(34236), "self-tagged video")
            .tag(Tag::identifier("video-id"))
            .tag(Tag::public_key(sender.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        assert!(video_mention_targets(&event).is_empty());
    }

    /// Build a `d=notify` list event tagging `creators`.
    fn notify_list_event(author: &Keys, creators: &[PublicKey]) -> Event {
        let mut builder =
            EventBuilder::new(Kind::from(30000), "").tag(Tag::identifier(NOTIFY_LIST_D_TAG));
        for creator in creators {
            builder = builder.tag(Tag::public_key(*creator));
        }
        builder.sign_with_keys(author).unwrap()
    }

    #[test]
    fn test_collect_notify_creators_reads_p_tags_in_order() {
        let author = Keys::generate();
        let first = Keys::generate().public_key();
        let second = Keys::generate().public_key();

        let event = notify_list_event(&author, &[first, second]);

        assert_eq!(collect_notify_creators(&event, 1000), vec![first, second]);
    }

    #[test]
    fn test_collect_notify_creators_deduplicates() {
        let author = Keys::generate();
        let creator = Keys::generate().public_key();

        let event = notify_list_event(&author, &[creator, creator, creator]);

        assert_eq!(collect_notify_creators(&event, 1000), vec![creator]);
    }

    #[test]
    fn test_collect_notify_creators_drops_self_reference() {
        let author = Keys::generate();
        let other = Keys::generate().public_key();

        let event = notify_list_event(&author, &[author.public_key(), other]);

        assert_eq!(
            collect_notify_creators(&event, 1000),
            vec![other],
            "belling yourself is meaningless"
        );
    }

    #[test]
    fn test_collect_notify_creators_handles_empty_list() {
        let author = Keys::generate();

        let event = notify_list_event(&author, &[]);

        assert!(
            collect_notify_creators(&event, 1000).is_empty(),
            "an empty list is legitimate, not malformed"
        );
    }

    #[test]
    fn test_collect_notify_creators_truncates_at_the_cap() {
        let author = Keys::generate();
        let creators: Vec<PublicKey> = (0..5).map(|_| Keys::generate().public_key()).collect();

        let event = notify_list_event(&author, &creators);
        let collected = collect_notify_creators(&event, 3);

        // Redis is single-threaded and the diff runs in one Lua script, so an
        // unbounded list would let one user stall the instance.
        assert_eq!(collected.len(), 3);
        assert_eq!(collected, creators[..3].to_vec(), "tag order is preserved");
    }

    #[test]
    fn test_collect_notify_creators_counts_unique_against_the_cap() {
        let author = Keys::generate();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();

        // Duplicates must not consume cap budget, or a list padded with repeats
        // would starve the real entries behind it.
        let event = notify_list_event(&author, &[a, a, a, b]);

        assert_eq!(collect_notify_creators(&event, 2), vec![a, b]);
    }

    #[test]
    fn test_new_post_copy_has_required_non_empty_body() {
        let (title, body) = new_post_copy("Alice");

        assert_eq!(title, "New vine");
        assert_eq!(body, "Alice posted a new vine");
        assert!(
            !body.trim().is_empty(),
            "mobile silently drops foreground pushes without a body"
        );
    }

    #[test]
    fn test_deliverable_targets_drops_the_author() {
        let author = Keys::generate().public_key();
        let other = Keys::generate().public_key();

        // A self-reaction p-tags its own sender, so the author arrives as a
        // recipient. Dropping them here rather than mid-loop keeps a self-only
        // event from resolving an event-scoped copy it cannot use.
        let targets = deliverable_targets(
            vec![
                NotificationTarget {
                    recipient: author,
                    notification_type: NotificationType::Like,
                },
                NotificationTarget {
                    recipient: other,
                    notification_type: NotificationType::Like,
                },
            ],
            &author,
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].recipient, other);
    }

    #[test]
    fn test_deliverable_targets_empties_a_self_only_event() {
        let author = Keys::generate().public_key();

        let targets = deliverable_targets(
            vec![NotificationTarget {
                recipient: author,
                notification_type: NotificationType::Repost,
            }],
            &author,
        );

        assert!(
            targets.is_empty(),
            "a self-only event must resolve to nothing deliverable"
        );
    }

    #[test]
    fn test_only_body_rendering_types_need_the_event_content() {
        // The content parse is skipped when no target renders the body, so this
        // has to stay in step with the arms of `create_fcm_payload` that call
        // `formatted_content()`. A type added to one and not the other silently
        // downgrades that push to raw, unparsed content.
        assert!(renders_event_content(NotificationType::Comment));
        assert!(renders_event_content(NotificationType::Mention));

        assert!(!renders_event_content(NotificationType::Like));
        assert!(!renders_event_content(NotificationType::Repost));
        assert!(!renders_event_content(NotificationType::NewPost));
    }

    #[test]
    fn test_needs_content_is_decided_over_the_whole_target_list() {
        // The copy resolves on whichever recipient clears the gates first, which
        // need not be one that renders the body. Deciding `needs_content` from
        // the full list keeps that ordering from silently downgrading a mention
        // to raw, unparsed content.
        let bell_first = LazyEventCopy::for_targets(&[
            NotificationTarget {
                recipient: Keys::generate().public_key(),
                notification_type: NotificationType::NewPost,
            },
            NotificationTarget {
                recipient: Keys::generate().public_key(),
                notification_type: NotificationType::Mention,
            },
        ]);
        assert!(bell_first.needs_content);

        let bells_only = LazyEventCopy::for_targets(&[NotificationTarget {
            recipient: Keys::generate().public_key(),
            notification_type: NotificationType::NewPost,
        }]);
        assert!(!bells_only.needs_content);
    }

    /// The summary must report a failure as a failure. Counting only pruned
    /// tokens meant a total outage — bad credentials, FCM 5xx, timeouts —
    /// logged `failed_count=0`, which reads as "nothing went wrong" in the
    /// first logs an operator checks. Reproduced live against a local relay.
    #[test]
    fn every_send_error_counts_as_a_failure_not_just_prunable_ones() {
        use crate::fcm_sender::FcmError;
        use std::time::Duration;

        assert_eq!(classify_send_outcome(&Ok(())), SendOutcome::Delivered);

        // A dead token is BOTH a failure and a prune.
        assert_eq!(
            classify_send_outcome(&Err(FcmError::TokenNotRegistered)),
            SendOutcome::FailedAndPrune
        );

        // Everything else is a failure and must not prune the token.
        for error in [
            FcmError::Unauthorized("bad credentials".into()),
            FcmError::InternalError,
            FcmError::RetryableInternal(Duration::from_secs(30)),
            FcmError::InvalidRequest("quota exceeded".into()),
            FcmError::InternalRequest("timed out".into()),
            FcmError::Unknown {
                code: 418,
                hint: None,
            },
        ] {
            assert_eq!(
                classify_send_outcome(&Err(error.clone())),
                SendOutcome::Failed,
                "{error:?} must count as failed and must not prune"
            );
        }
    }

    #[tokio::test]
    async fn test_a_recipient_without_tokens_never_resolves_the_event_copy() {
        // The regression this guards is a throughput one, but the observable
        // fact is binary: did the event pay for its copy at all? With no mention
        // parser configured the resolve is free, so this asserts *whether* it
        // happened, not what it cost. In production it costs a Redis GET and,
        // on a cache miss, a relay round trip that is never negatively cached.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let recipient = Keys::generate().public_key();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(MockFcmSender::new())),
        );
        let event = EventBuilder::text_note("hello")
            .tag(Tag::public_key(recipient))
            .sign_with_keys(&author)
            .unwrap();

        let copy = LazyEventCopy::for_targets(&[NotificationTarget {
            recipient,
            notification_type: NotificationType::Mention,
        }]);
        send_notification_to_user(
            &state,
            &event,
            &recipient,
            NotificationType::Mention,
            &copy,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(
            copy.cell.get().is_none(),
            "an event with no deliverable recipient must not resolve its copy"
        );
    }

    #[tokio::test]
    async fn test_a_delivered_push_resolves_the_event_copy() {
        // The other half of the pair: deferring must not skip the resolve for a
        // recipient that does get a push.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let recipient = Keys::generate();
        let fcm_token = format!("lazy-copy-{}", recipient.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::text_note("hello")
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&author)
            .unwrap();

        let copy = LazyEventCopy::for_targets(&[NotificationTarget {
            recipient: recipient.public_key(),
            notification_type: NotificationType::Mention,
        }]);
        send_notification_to_user(
            &state,
            &event,
            &recipient.public_key(),
            NotificationType::Mention,
            &copy,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(mock_sender.get_sent_messages().len(), 1);
        assert_eq!(
            copy.cell.get().map(|c| c.sender_name.as_str()),
            Some(format_short_npub(&author.public_key()).as_str()),
            "a delivered push resolves the copy the payload is built from"
        );

        redis_store::remove_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();
    }

    /// The sweep in `cleanup_stale_tokens` reads this score and nothing else.
    /// Documented in AGENTS.md; the constant is private to `redis_store`.
    async fn staleness_score(pool: &redis_store::RedisPool, token: &str) -> Option<u64> {
        let mut conn = pool.get().await.unwrap();
        let score: Option<f64> = redis::cmd("ZSCORE")
            .arg("stale_tokens")
            .arg(token)
            .query_async(&mut *conn)
            .await
            .unwrap();
        score.map(|s| s as u64)
    }

    async fn backdate_staleness_score(pool: &redis_store::RedisPool, token: &str, score: u64) {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZADD")
            .arg("stale_tokens")
            .arg(score)
            .arg(token)
            .query_async::<i64>(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_delivered_push_refreshes_the_tokens_staleness_score() {
        // A device that keeps receiving pushes is by definition not stale, so
        // the sweep must not delete it 90 days after its last *registration*.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let recipient = Keys::generate();
        let fcm_token = format!("staleness-live-{}", recipient.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();

        let registered_at = Timestamp::now().as_secs() - 80 * 24 * 60 * 60;
        backdate_staleness_score(&pool, &fcm_token, registered_at).await;

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::text_note("hello")
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&author)
            .unwrap();

        send_notification_to_user(
            &state,
            &event,
            &recipient.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(mock_sender.get_sent_messages().len(), 1);
        let score = staleness_score(&pool, &fcm_token)
            .await
            .expect("a delivered token stays tracked");
        assert!(
            score > registered_at,
            "a delivered push left the token at its registration score ({score}), \
             so the sweep still measures age rather than inactivity"
        );

        redis_store::remove_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_failed_send_leaves_the_staleness_score_alone() {
        // Only a delivered push is evidence the device is alive. An auth
        // failure or an FCM outage says nothing about the token, and treating
        // it as activity would keep dead tokens alive forever.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let recipient = Keys::generate();
        let fcm_token = format!("staleness-failed-{}", recipient.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();

        let registered_at = Timestamp::now().as_secs() - 80 * 24 * 60 * 60;
        backdate_staleness_score(&pool, &fcm_token, registered_at).await;

        let mock_sender = MockFcmSender::new();
        mock_sender.set_error_for_token(
            &fcm_token,
            FcmError::Unauthorized("credentials rejected".to_string()),
        );
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::text_note("hello")
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&author)
            .unwrap();

        send_notification_to_user(
            &state,
            &event,
            &recipient.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            staleness_score(&pool, &fcm_token).await,
            Some(registered_at),
            "a failed send must not count as proof the device is alive"
        );

        redis_store::remove_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();
    }

    #[test]
    fn test_payload_uses_the_event_scoped_sender_name() {
        let author = Keys::generate();
        let recipient = Keys::generate().public_key();
        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "a new vine")
            .tag(Tag::identifier("hoisted-sender"))
            .sign_with_keys(&author)
            .unwrap();
        let copy = EventScopedCopy {
            sender_name: "Alice".to_string(),
            formatted_content: None,
        };

        let payload = create_fcm_payload(&event, &recipient, NotificationType::NewPost, &copy);
        let data = payload.data.expect("data-only payload");

        // The name resolved once for the event reaches the per-recipient push.
        assert_eq!(data.get("senderName"), Some(&"Alice".to_string()));
        assert_eq!(data.get("title"), Some(&"New vine".to_string()));
        assert_eq!(
            data.get("body"),
            Some(&"Alice posted a new vine".to_string())
        );
    }

    #[test]
    fn test_mention_body_prefers_the_resolved_content() {
        let author = Keys::generate();
        let recipient = Keys::generate().public_key();
        let event = EventBuilder::text_note("hey nostr:npub1raw")
            .sign_with_keys(&author)
            .unwrap();
        let copy = EventScopedCopy {
            sender_name: "Alice".to_string(),
            formatted_content: Some("hey @bob".to_string()),
        };

        let payload = create_fcm_payload(&event, &recipient, NotificationType::Mention, &copy);
        let data = payload.data.expect("data-only payload");

        assert_eq!(data.get("body"), Some(&"Alice: hey @bob".to_string()));
    }

    #[test]
    fn test_mention_body_falls_back_to_raw_content() {
        let author = Keys::generate();
        let recipient = Keys::generate().public_key();
        let event = EventBuilder::text_note("hey nostr:npub1raw")
            .sign_with_keys(&author)
            .unwrap();
        // `None` is what a skipped or failed parse leaves behind. Before the
        // hoist the per-recipient parse fell back to the raw content on error;
        // that behaviour has to survive the move.
        let copy = EventScopedCopy {
            sender_name: "Alice".to_string(),
            formatted_content: None,
        };

        let payload = create_fcm_payload(&event, &recipient, NotificationType::Mention, &copy);
        let data = payload.data.expect("data-only payload");

        assert_eq!(
            data.get("body"),
            Some(&"Alice: hey nostr:npub1raw".to_string())
        );
    }

    #[tokio::test]
    async fn test_a_real_watcher_page_failure_still_delivers_mentions() {
        // This drives a genuine watcher-page failure through the real video
        // delivery path by leaving a string where the watcher set belongs, so
        // SSCAN fails with WRONGTYPE. Mentions came from the event's own tags
        // and must still be delivered.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let _fanout_guard = fanout_test_lock().lock().await;
        let author = Keys::generate();
        let mentioned = Keys::generate().public_key();
        let fcm_token = format!("mention_token_{}", EventId::all_zeros().to_hex());
        redis_store::add_or_update_token(&pool, &mentioned, &fcm_token)
            .await
            .expect("register token");
        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "video")
            .tag(Tag::identifier("wrongtype-vid"))
            .tag(Tag::public_key(mentioned))
            .sign_with_keys(&author)
            .unwrap();

        let watchers_key = format!("notify_watchers:{}", author.public_key().to_hex());
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("SET")
            .arg(&watchers_key)
            .arg("not-a-set")
            .query_async(&mut *conn)
            .await
            .unwrap();

        // Without this the test could pass vacuously, by the lookup succeeding
        // and simply finding no watchers.
        let lookup_failed =
            redis_store::get_notify_watchers_page(&pool, &author.public_key(), 0, 1000)
                .await
                .is_err();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        handle_video_content_event(&state, &event, CancellationToken::new())
            .await
            .expect("video handling degrades to mentions");

        let _: () = redis::cmd("DEL")
            .arg(&watchers_key)
            .arg("new_post_fanout_jobs")
            .arg(format!("fanout:enqueued:{}", event.id.to_hex()))
            .query_async(&mut *conn)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &mentioned, &fcm_token)
            .await
            .expect("cleanup token");

        assert!(lookup_failed, "the seeded key must make the lookup fail");
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "the mention survives the failed watcher page lookup"
        );
    }

    #[tokio::test]
    async fn test_a_cancelled_bell_page_reports_cancellation() {
        // Deleting the `Cancelled` arm from `send_notifications_bounded` left
        // the suite green, so shutdown on the bell path was propagating by
        // nobody's assertion. The page must report cancellation upward rather
        // than returning Ok and letting the walk advance to the next cursor.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "video")
            .tag(Tag::identifier("cancelled-page-vid"))
            .sign_with_keys(&author)
            .unwrap();
        let targets: Vec<NotificationTarget> = (0..3)
            .map(|_| NotificationTarget {
                recipient: Keys::generate().public_key(),
                notification_type: NotificationType::NewPost,
            })
            .collect();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let token = CancellationToken::new();
        token.cancel();

        let result = send_notifications_bounded(
            &state,
            &event,
            targets,
            &LazyEventCopy::for_targets(&[]),
            token,
            4,
        )
        .await;

        assert!(
            matches!(result, Err(crate::error::ServiceError::Cancelled)),
            "a cancelled page must not report success"
        );
        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "nothing ships after cancellation"
        );
    }

    #[test]
    fn test_a_watcher_page_types_every_watcher_as_new_post() {
        let author = Keys::generate().public_key();
        let watcher = Keys::generate().public_key();

        let targets = watcher_page_targets(vec![watcher], &author, &HashSet::new());

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].recipient, watcher);
        assert_eq!(targets[0].notification_type, NotificationType::NewPost);
    }

    #[test]
    fn test_a_watcher_page_drops_an_author_watching_themselves() {
        let author = Keys::generate().public_key();

        let targets = watcher_page_targets(vec![author], &author, &HashSet::new());

        assert!(targets.is_empty());
    }

    #[test]
    fn test_a_mentioned_watcher_is_not_also_belled() {
        // Mention wins on overlap, which is what README and the developer guide
        // promise. The rule moved onto the page path with the fan-out rewrite
        // and arrived there without a test: without one,
        // deleting the `mentioned` filter leaves the whole suite green while
        // every mentioned watcher gets two pushes for one video.
        let author = Keys::generate().public_key();
        let both = Keys::generate().public_key();
        let mentioned = HashSet::from([both]);

        let targets = watcher_page_targets(vec![both], &author, &mentioned);

        assert!(
            targets.is_empty(),
            "the mention already covers this recipient, so no bell is owed"
        );
    }

    #[tokio::test]
    async fn test_the_fan_out_reaches_watchers_past_the_first_page() {
        // The paging loop is the point of the fan-out bound, and nothing failed
        // when it was broken: stopping after page one, or never advancing the
        // cursor, left the suite green. It needs a creator whose watcher set is
        // big enough for Redis to page at all. Below Redis's
        // `set-max-listpack-entries` (128 by default) a set is listpack-encoded
        // and SSCAN returns every member in one reply whatever COUNT says, so a
        // handful of watchers cannot exercise a second page.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let _fanout_guard = fanout_test_lock().lock().await;
        const WATCHERS: usize = 150;
        const PAGE_SIZE: usize = 10;

        let author = Keys::generate();
        let creator = author.public_key();
        let watchers: Vec<Keys> = (0..WATCHERS).map(|_| Keys::generate()).collect();

        for (idx, watcher) in watchers.iter().enumerate() {
            redis_store::add_or_update_token(
                &pool,
                &watcher.public_key(),
                &format!("fanout_token_{}", idx),
            )
            .await
            .expect("register token");
            redis_store::replace_notify_subscriptions(
                &pool,
                &watcher.public_key(),
                &[creator],
                1_000 + idx as u64,
                &EventId::all_zeros(),
            )
            .await
            .expect("bell the creator");
        }

        // Without this the assertion below could be met by one oversized page.
        let first_page = redis_store::get_notify_watchers_page(&pool, &creator, 0, PAGE_SIZE)
            .await
            .expect("first watcher page");
        assert_ne!(
            first_page.next_cursor, 0,
            "the seeded set must be large enough for Redis to page"
        );

        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.new_post_fanout_page_size = PAGE_SIZE;
        settings.service.new_post_delivery_concurrency = 4;
        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "video")
            .tag(Tag::identifier("paged-fanout-vid"))
            .sign_with_keys(&author)
            .unwrap();

        let result = handle_video_content_event(&state, &event, CancellationToken::new()).await;
        result.expect("the handler queues fan-out");
        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "watcher delivery must not run on the event-handler loop"
        );

        for _ in 0..100 {
            let Some(job_json) = redis_store::claim_fanout_job(
                &pool,
                state.settings.service.new_post_fanout_lease_secs,
            )
            .await
            .expect("claim durable page") else {
                break;
            };
            process_fanout_job(&state, &job_json, CancellationToken::new())
                .await
                .expect("process durable page");
            if mock_sender.get_sent_messages().len() == WATCHERS {
                break;
            }
        }

        for (idx, watcher) in watchers.iter().enumerate() {
            let _ = redis_store::remove_token(
                &pool,
                &watcher.public_key(),
                &format!("fanout_token_{}", idx),
            )
            .await;
            // The coordinate claim carries a one-year TTL, so it has to go with
            // the rest or every run leaves 150 keys behind for a year.
            let claim_key =
                video_recipient_claim_key(&event, &watcher.public_key(), NotificationType::NewPost)
                    .expect("the test event has a d-tag");
            let mut conn = pool.get().await.unwrap();
            let _: () = redis::cmd("DEL")
                .arg(format!("notify_subs:{}", watcher.public_key().to_hex()))
                .arg(format!("notify_subs_ts:{}", watcher.public_key().to_hex()))
                .arg(redis_store::build_notify_rate_key(
                    &watcher.public_key(),
                    &creator,
                ))
                .arg(format!("dedup:{claim_key}"))
                .arg(format!(
                    "dedup:{}:{}",
                    event.id.to_hex(),
                    watcher.public_key().to_hex()
                ))
                .query_async(&mut *conn)
                .await
                .unwrap();
        }
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("DEL")
            .arg(format!("notify_watchers:{}", creator.to_hex()))
            .arg("new_post_fanout_jobs")
            .arg(format!("fanout:enqueued:{}", event.id.to_hex()))
            .query_async(&mut *conn)
            .await
            .unwrap();

        assert_eq!(
            mock_sender.get_sent_messages().len(),
            WATCHERS,
            "every watcher is notified, not just the ones on the first page"
        );
    }

    #[tokio::test]
    async fn recipient_claims_recover_partial_delivery_without_resending_successes() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let first = Keys::generate().public_key();
        let second = Keys::generate().public_key();
        let first_token = format!("partial-first-{}", first.to_hex());
        let second_token = format!("partial-second-{}", second.to_hex());
        redis_store::add_or_update_token(&pool, &first, &first_token)
            .await
            .unwrap();
        redis_store::add_or_update_token(&pool, &second, &second_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::text_note("partial delivery")
            .tag(Tag::public_key(first))
            .tag(Tag::public_key(second))
            .sign_with_keys(&author)
            .unwrap();

        send_notification_to_user(
            &state,
            &event,
            &first,
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let targets = targets_of(NotificationType::Mention, vec![first, second]);
        send_notifications_sequential(
            &state,
            &event,
            targets,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            mock_sender.get_sent_messages().len(),
            2,
            "replay reaches the untouched recipient without duplicating the completed one"
        );

        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{}:{}", event.id.to_hex(), first.to_hex()))
            .arg(format!("dedup:{}:{}", event.id.to_hex(), second.to_hex()))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &first, &first_token)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &second, &second_token)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retryable_recipient_failure_releases_only_that_recipient_claim() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let first = Keys::generate().public_key();
        let second = Keys::generate().public_key();
        let first_token = format!("retry-first-{}", first.to_hex());
        let second_token = format!("retry-second-{}", second.to_hex());
        redis_store::add_or_update_token(&pool, &first, &first_token)
            .await
            .unwrap();
        redis_store::add_or_update_token(&pool, &second, &second_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        mock_sender.set_error_for_token(
            &second_token,
            FcmError::RetryableInternal(Duration::from_secs(1)),
        );
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::text_note("retryable partial delivery")
            .tag(Tag::public_key(first))
            .tag(Tag::public_key(second))
            .sign_with_keys(&author)
            .unwrap();
        let targets = targets_of(NotificationType::Mention, vec![first, second]);

        let first_pass = send_notifications_sequential(
            &state,
            &event,
            targets.clone(),
            &test_copy(),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            first_pass,
            Err(crate::error::ServiceError::RetryableDelivery(_))
        ));
        assert_eq!(mock_sender.get_sent_messages().len(), 1);

        mock_sender.clear();
        send_notifications_sequential(
            &state,
            &event,
            targets.clone(),
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "only the failed recipient retries"
        );

        send_notifications_sequential(
            &state,
            &event,
            targets,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "successful retry is retained and cannot deliver twice"
        );

        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{}:{}", event.id.to_hex(), first.to_hex()))
            .arg(format!("dedup:{}:{}", event.id.to_hex(), second.to_hex()))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &first, &first_token)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &second, &second_token)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn one_success_retains_the_recipient_claim_when_a_sibling_token_is_retryable() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let recipient = Keys::generate().public_key();
        let live_token = format!("mixed-live-{}", recipient.to_hex());
        let retryable_token = format!("mixed-retryable-{}", recipient.to_hex());
        redis_store::add_or_update_token(&pool, &recipient, &live_token)
            .await
            .unwrap();
        redis_store::add_or_update_token(&pool, &recipient, &retryable_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        mock_sender.set_error_for_token(
            &retryable_token,
            FcmError::RetryableInternal(Duration::from_secs(1)),
        );
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::text_note("mixed token delivery")
            .tag(Tag::public_key(recipient))
            .sign_with_keys(&author)
            .unwrap();
        let targets = targets_of(NotificationType::Mention, vec![recipient]);

        send_notifications_sequential(
            &state,
            &event,
            targets.clone(),
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(mock_sender.get_sent_messages().len(), 1);

        mock_sender.clear();
        send_notifications_sequential(
            &state,
            &event,
            targets,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "recipient replay must not duplicate the token that already succeeded"
        );

        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!(
                "dedup:{}:{}",
                event.id.to_hex(),
                recipient.to_hex()
            ))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &recipient, &live_token)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &recipient, &retryable_token)
            .await
            .unwrap();
    }

    #[test]
    fn test_a_watcher_page_keeps_the_watchers_who_were_not_mentioned() {
        // Partial overlap: the mention pass already covers `mentioned`, so the
        // page owes a bell to `watcher` and nothing to them.
        let author = Keys::generate().public_key();
        let mentioned = Keys::generate().public_key();
        let watcher = Keys::generate().public_key();

        let targets = watcher_page_targets(
            vec![mentioned, watcher],
            &author,
            &HashSet::from([mentioned]),
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].recipient, watcher);
        assert_eq!(targets[0].notification_type, NotificationType::NewPost);
    }

    #[test]
    fn durable_fanout_retry_backoff_is_bounded() {
        assert_eq!(fanout_retry_delay(5, 1, 0), 5);
        assert_eq!(fanout_retry_delay(5, 2, 0), 10);
        assert_eq!(fanout_retry_delay(5, 7, 0), 300);
        assert_eq!(fanout_retry_delay(5, u16::MAX, 0), 300);
        assert_eq!(fanout_retry_delay(5, 1, 120), 120);
        assert_eq!(fanout_retry_delay(5, 1, 3_600), 3_600);
    }

    #[test]
    fn durable_fanout_retry_persists_each_attempt() {
        let initial = NewPostFanoutJob {
            event_json: "{}".to_string(),
            cursor: 17,
            sender_name: Some("Alice".to_string()),
            attempt: 0,
            expires_at: 1_000,
        };

        let (first, first_delay) = next_fanout_retry(&initial, 5, 0, 0).unwrap();
        let persisted = serde_json::to_string(&first).unwrap();
        let restored: NewPostFanoutJob = serde_json::from_str(&persisted).unwrap();
        let (second, second_delay) = next_fanout_retry(&restored, 5, 0, 0).unwrap();

        assert_eq!(first.attempt, 1);
        assert_eq!(first_delay, 5);
        assert_eq!(second.attempt, 2);
        assert_eq!(second_delay, 10);
        assert_eq!(second.cursor, initial.cursor);
        assert_eq!(second.expires_at, initial.expires_at);
    }

    #[test]
    fn durable_fanout_retry_stops_after_twelve_delivery_attempts() {
        let mut job = NewPostFanoutJob {
            event_json: "{}".to_string(),
            cursor: 17,
            sender_name: None,
            attempt: 0,
            expires_at: 1_000,
        };

        for expected_attempt in 1..MAX_FANOUT_ATTEMPTS {
            let (retry, _) = next_fanout_retry(&job, 5, 0, 0).unwrap();
            assert_eq!(retry.attempt, expected_attempt);
            job = retry;
        }

        assert!(next_fanout_retry(&job, 5, 0, 0).is_none());
    }

    #[test]
    fn durable_fanout_retry_preserves_provider_floor_within_job_lifetime() {
        let job = NewPostFanoutJob {
            event_json: "{}".to_string(),
            cursor: 17,
            sender_name: None,
            attempt: 0,
            expires_at: 10_000,
        };

        let (_, provider_delay) = next_fanout_retry(&job, 5, 3_600, 100).unwrap();
        let (_, lifetime_clamped_delay) = next_fanout_retry(&job, 5, u64::MAX, 100).unwrap();

        assert_eq!(provider_delay, 3_600);
        assert_eq!(lifetime_clamped_delay, 9_900);
    }

    #[tokio::test]
    async fn exhausted_fanout_page_preserves_its_known_successor() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let _guard = fanout_test_lock().lock().await;
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(MockFcmSender::new())),
        );
        let current = NewPostFanoutJob {
            event_json: "{}".to_string(),
            cursor: 17,
            sender_name: None,
            attempt: MAX_FANOUT_ATTEMPTS - 1,
            expires_at: Timestamp::now().as_secs().saturating_add(3_600),
        };
        let successor = NewPostFanoutJob {
            cursor: 23,
            attempt: 0,
            ..current.clone()
        };
        let current_json = serde_json::to_string(&current).unwrap();
        let successor_json = serde_json::to_string(&successor).unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZADD")
            .arg("new_post_fanout_jobs")
            .arg(0)
            .arg(&current_json)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        schedule_fanout_retry(
            &state,
            &current_json,
            &current,
            0,
            "delivery",
            Some(&successor_json),
        )
        .await
        .unwrap();

        let mut conn = pool.get().await.unwrap();
        let current_score: Option<f64> = redis::cmd("ZSCORE")
            .arg("new_post_fanout_jobs")
            .arg(&current_json)
            .query_async(&mut *conn)
            .await
            .unwrap();
        let successor_score: Option<f64> = redis::cmd("ZSCORE")
            .arg("new_post_fanout_jobs")
            .arg(&successor_json)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(current_score.is_none());
        assert!(
            successor_score
                .is_some_and(|score| score > chrono::Utc::now().timestamp_millis() as f64),
            "the successor should inherit backoff instead of becoming immediately claimable"
        );
        redis::cmd("ZREM")
            .arg("new_post_fanout_jobs")
            .arg(&successor_json)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[test]
    fn durable_fanout_job_expires_at_its_deadline() {
        let job = NewPostFanoutJob {
            event_json: "{}".to_string(),
            cursor: 0,
            sender_name: None,
            attempt: 9,
            expires_at: 100,
        };

        assert!(!fanout_job_expired(&job, 99));
        assert!(fanout_job_expired(&job, 100));
        assert!(fanout_job_expired(&job, 101));
    }

    #[test]
    fn test_video_recipient_claim_key_is_stable_across_edits() {
        let owner = Keys::generate();
        let recipient = Keys::generate();
        let added_recipient = Keys::generate();
        let first_event = EventBuilder::new(Kind::from(KIND_VIDEO), "first version")
            .tag(Tag::identifier("video:d-tag"))
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        let edited_event = EventBuilder::new(Kind::from(KIND_VIDEO), "edited version")
            .tag(Tag::identifier("video:d-tag"))
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&owner)
            .unwrap();

        assert_ne!(first_event.id, edited_event.id);
        assert_eq!(
            video_recipient_claim_key(
                &first_event,
                &recipient.public_key(),
                NotificationType::Mention
            ),
            video_recipient_claim_key(
                &edited_event,
                &recipient.public_key(),
                NotificationType::Mention
            )
        );
        assert_eq!(
            video_recipient_claim_key(
                &first_event,
                &recipient.public_key(),
                NotificationType::Mention
            ),
            Some(format!(
                "34236:mention:{}:video:d-tag:{}",
                owner.public_key().to_hex(),
                recipient.public_key().to_hex()
            ))
        );
        assert_ne!(
            video_recipient_claim_key(
                &edited_event,
                &recipient.public_key(),
                NotificationType::Mention
            ),
            video_recipient_claim_key(
                &edited_event,
                &added_recipient.public_key(),
                NotificationType::Mention
            ),
            "a newly added recipient must have an independent delivery record"
        );
    }

    #[test]
    fn test_video_recipient_claim_key_separates_notification_types() {
        let owner = Keys::generate();
        let recipient = Keys::generate();
        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "a new vine")
            .tag(Tag::identifier("video:d-tag"))
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&owner)
            .unwrap();

        // A bell and a mention for the same coordinate are different pushes
        // carrying different information, so one must not consume the other's
        // record for the coordinate TTL — a year by default.
        assert_ne!(
            video_recipient_claim_key(&event, &recipient.public_key(), NotificationType::NewPost),
            video_recipient_claim_key(&event, &recipient.public_key(), NotificationType::Mention),
            "a bell must not suppress a later mention on the same video"
        );
    }

    #[tokio::test]
    async fn test_video_coordinate_is_recorded_only_after_successful_delivery() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let recipient = Keys::generate();
        let fcm_token = format!("video-dedup-token-{}", recipient.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        mock_sender.set_error_for_token(&fcm_token, FcmError::InternalError);
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let first_event = EventBuilder::new(Kind::from(KIND_VIDEO), "first version")
            .tag(Tag::identifier("post-success-dedup"))
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        let claim_key = video_recipient_claim_key(
            &first_event,
            &recipient.public_key(),
            NotificationType::Mention,
        )
        .unwrap();
        let redis_key = format!("dedup:{claim_key}");

        send_notification_to_user(
            &state,
            &first_event,
            &recipient.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            redis_store::get_cached_string(&pool, &redis_key)
                .await
                .unwrap(),
            None,
            "a failed FCM send must not mark the video recipient as notified"
        );

        mock_sender.clear();
        let successful_edit = EventBuilder::new(Kind::from(KIND_VIDEO), "successful edit")
            .tag(Tag::identifier("post-success-dedup"))
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &successful_edit,
            &recipient.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(mock_sender.get_sent_messages().len(), 1);
        assert_eq!(
            redis_store::get_cached_string(&pool, &redis_key)
                .await
                .unwrap(),
            Some("1".to_string()),
            "a successful FCM send must mark the video recipient as notified"
        );

        let later_edit = EventBuilder::new(Kind::from(KIND_VIDEO), "later edit")
            .tag(Tag::identifier("post-success-dedup"))
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &later_edit,
            &recipient.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "a recorded coordinate must suppress later edits for the same recipient"
        );

        redis_store::remove_token(&pool, &recipient.public_key(), &fcm_token)
            .await
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(redis_key)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[test]
    fn test_a_mention_satisfies_the_bell_record_but_not_the_reverse() {
        // The asymmetry is the whole point: a mention names the video, so it
        // covers the bell, but a bell says nothing about being mentioned.
        assert_eq!(
            satisfied_video_claims(NotificationType::Mention),
            vec![NotificationType::Mention, NotificationType::NewPost]
        );
        assert_eq!(
            satisfied_video_claims(NotificationType::NewPost),
            vec![NotificationType::NewPost]
        );
        assert_eq!(
            satisfied_video_claims(NotificationType::Comment),
            vec![NotificationType::Comment]
        );
    }

    #[tokio::test]
    async fn test_a_mention_suppresses_a_later_bell_on_the_same_video() {
        // The other direction of the same rule. `watcher_page_targets` already
        // says mention wins over bell on overlap; that has to hold across edits
        // too. A watcher who was `p`-tagged in the original and dropped from the
        // edit resolves to a bare NewPost target on the edit, and without the
        // mention's record standing in for it they are told "posted a new vine"
        // about a video they were already pushed about.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("mention-then-bell-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        // The watcher is `p`-tagged on the original, so mention wins and the
        // bell is never delivered for this coordinate.
        let published = EventBuilder::new(Kind::from(KIND_VIDEO), "first version")
            .tag(Tag::identifier("mention-then-bell"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &published,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "the mention delivers"
        );

        // The creator edits the video and drops the `p` tag. The watcher is now
        // only a bell target for the same coordinate.
        let edit_dropping_mention = EventBuilder::new(Kind::from(KIND_VIDEO), "edited version")
            .tag(Tag::identifier("mention-then-bell"))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &edit_dropping_mention,
            &watcher.public_key(),
            NotificationType::NewPost,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "an edit must not re-announce an already-pushed video as a new post"
        );

        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();
        let bell_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::NewPost)
                .unwrap();
        let mention_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::Mention)
                .unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{bell_key}"))
            .arg(format!("dedup:{mention_key}"))
            .arg(redis_store::build_notify_rate_key(
                &watcher.public_key(),
                &owner.public_key(),
            ))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_legacy_video_claim_suppresses_type_scoped_delivery() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("legacy-claim-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "edited version")
            .tag(Tag::identifier("legacy-claim"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        let legacy_key = legacy_video_recipient_claim_key(&event, &watcher.public_key()).unwrap();
        redis_store::set_cached_string(&pool, &format!("dedup:{legacy_key}"), "1", 3600)
            .await
            .unwrap();

        send_notification_to_user(
            &state,
            &event,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "a one-year legacy coordinate record must survive the key-format rollout"
        );

        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{legacy_key}"))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_muted_video_mention_falls_back_to_enabled_bell_for_watcher() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("mention-fallback-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();
        preferences::set_user_preferences(
            &pool,
            &watcher.public_key().to_hex(),
            &UserPreferences { kinds: vec![34236] },
        )
        .await
        .unwrap();
        redis_store::replace_notify_subscriptions(
            &pool,
            &watcher.public_key(),
            &[owner.public_key()],
            1000,
            &test_event_id(1000),
        )
        .await
        .unwrap();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );
        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "mentioned watcher")
            .tag(Tag::identifier("mention-fallback"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();

        send_notification_to_user(
            &state,
            &event,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let sent = mock_sender.get_sent_messages();
        assert_eq!(sent.len(), 1);
        let payload_type = sent[0].1.data.as_ref().and_then(|data| data.get("type"));
        assert_eq!(
            payload_type,
            Some(&"newPost".to_string()),
            "the muted mention should deliver the bell the watcher enabled"
        );

        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();
        preferences::delete_user_preferences(&pool, &watcher.public_key().to_hex())
            .await
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        let bell_key =
            video_recipient_claim_key(&event, &watcher.public_key(), NotificationType::NewPost)
                .unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{bell_key}"))
            .arg(redis_store::build_notify_rate_key(
                &watcher.public_key(),
                &owner.public_key(),
            ))
            .arg(format!("notify_subs:{}", watcher.public_key().to_hex()))
            .arg(format!("notify_subs_ts:{}", watcher.public_key().to_hex()))
            .arg(format!("notify_watchers:{}", owner.public_key().to_hex()))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_rate_limited_bell_records_the_video_coordinate() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("rate-limited-claim-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let rate_key =
            redis_store::build_notify_rate_key(&watcher.public_key(), &owner.public_key());
        redis_store::set_cached_string(&pool, &rate_key, "1", 3600)
            .await
            .unwrap();
        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "inside the window")
            .tag(Tag::identifier("rate-limited-claim"))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &event,
            &watcher.public_key(),
            NotificationType::NewPost,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let claim_key =
            video_recipient_claim_key(&event, &watcher.public_key(), NotificationType::NewPost)
                .unwrap();
        let record = redis_store::get_cached_string(&pool, &format!("dedup:{claim_key}"))
            .await
            .unwrap();
        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "the rate-limited video itself is suppressed"
        );
        assert!(
            record.is_some(),
            "a suppressed new-post push must still mark the coordinate"
        );

        let edit = EventBuilder::new(Kind::from(KIND_VIDEO), "later edit")
            .tag(Tag::identifier("rate-limited-claim"))
            .sign_with_keys(&owner)
            .unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(&rate_key)
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
        send_notification_to_user(
            &state,
            &edit,
            &watcher.public_key(),
            NotificationType::NewPost,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "a later edit of the suppressed video must stay quiet"
        );

        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{claim_key}"))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_bookkeeping_failure_does_not_skip_the_video_claim() {
        // The post-send writes are bookkeeping: the push has already shipped, so
        // one of them failing must not take the others down with it. The
        // coordinate record is what stops a NIP-33 edit re-notifying, so losing
        // it because an unrelated write errored costs the user a duplicate push.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("bookkeeping-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        // `SETEX key 0 v` is a Redis error, so the rate-limit write fails while
        // the send itself succeeds. Reachable in production via
        // `NOSTR_PUSH__SERVICE__NEW_POST_RATE_LIMIT_SECS=0`, which nothing
        // validates at load.
        let mut settings = crate::config::Settings::new().unwrap();
        settings.service.new_post_rate_limit_secs = 0;
        let state = test_app_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let published = EventBuilder::new(Kind::from(KIND_VIDEO), "a new vine")
            .tag(Tag::identifier("bookkeeping-vid"))
            .sign_with_keys(&owner)
            .unwrap();
        let result = send_notification_to_user(
            &state,
            &published,
            &watcher.public_key(),
            NotificationType::NewPost,
            &test_copy(),
            CancellationToken::new(),
        )
        .await;

        let claim_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::NewPost)
                .unwrap();
        let record = redis_store::get_cached_string(&pool, &format!("dedup:{claim_key}"))
            .await
            .unwrap();

        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("DEL")
            .arg(format!("dedup:{claim_key}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "the push was delivered"
        );
        assert!(
            record.is_some(),
            "a delivered video push must record its coordinate, or the next edit re-notifies"
        );
        assert!(result.is_ok(), "a delivered push must not report failure");
    }

    #[tokio::test]
    async fn test_a_failed_claim_write_is_not_reported_as_a_delivery_failure() {
        // Same class as the test above, one write further down: the claim loop.
        // A delivered push that returns `Err` is not just cosmetic. The caller
        // logs it as a failed notification, and the remaining post-send work,
        // including invalid-token removal, is skipped.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("claim-write-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        let mut settings = crate::config::Settings::new().unwrap();
        // Makes the coordinate `SETEX` error while the send still succeeds.
        settings.service.video_coordinate_dedup_ttl_secs = 0;
        let state = test_app_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let published = EventBuilder::new(Kind::from(KIND_VIDEO), "a new vine")
            .tag(Tag::identifier("claim-write-vid"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        let result = send_notification_to_user(
            &state,
            &published,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await;

        // Without this the test could pass vacuously, by the claim write
        // quietly succeeding and there being nothing to survive.
        let claim_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::Mention)
                .unwrap();
        let record = redis_store::get_cached_string(&pool, &format!("dedup:{claim_key}"))
            .await
            .unwrap();

        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "the push was delivered"
        );
        assert!(
            record.is_none(),
            "the claim write must actually have failed, or this test proves nothing"
        );
        assert!(
            result.is_ok(),
            "a bookkeeping write failing is not a delivery failure"
        );
    }

    #[tokio::test]
    async fn test_a_failed_claim_write_still_removes_the_invalid_token() {
        // The consequence the test above only implies. Invalid-token removal
        // runs after the claim writes, so a `?` on them strands a token FCM has
        // already rejected: every later push to this user pays for a delivery
        // that cannot land, and nothing retries the removal.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        // Two tokens: one has to succeed, or `success_count` stays 0 and the
        // claim write this test needs to fail never runs at all.
        let live_token = format!("live-{}", watcher.public_key().to_hex());
        let stale_token = format!("stale-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &live_token)
            .await
            .unwrap();
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &stale_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        mock_sender.set_error_for_token(&stale_token, FcmError::TokenNotRegistered);
        let mut settings = crate::config::Settings::new().unwrap();
        // Makes both coordinate `SETEX` calls error while the send succeeds.
        settings.service.video_coordinate_dedup_ttl_secs = 0;
        let state = test_app_state(
            settings,
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let published = EventBuilder::new(Kind::from(KIND_VIDEO), "a new vine")
            .tag(Tag::identifier("stale-token-vid"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        let result = send_notification_to_user(
            &state,
            &published,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await;

        let claim_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::Mention)
                .unwrap();
        let record = redis_store::get_cached_string(&pool, &format!("dedup:{claim_key}"))
            .await
            .unwrap();
        let remaining = redis_store::get_tokens_for_pubkey(&pool, &watcher.public_key())
            .await
            .unwrap();

        redis_store::remove_token(&pool, &watcher.public_key(), &live_token)
            .await
            .unwrap();
        redis_store::remove_token(&pool, &watcher.public_key(), &stale_token)
            .await
            .unwrap();

        assert!(
            record.is_none(),
            "the claim write must actually have failed, or this test proves nothing"
        );
        assert!(
            result.is_ok(),
            "a bookkeeping failure is not a send failure"
        );
        assert!(
            !remaining.contains(&stale_token),
            "a token FCM reported as unregistered must be removed even when the claim write failed"
        );
        assert!(
            remaining.contains(&live_token),
            "the token that delivered must survive"
        );
    }

    #[tokio::test]
    async fn test_a_bell_does_not_suppress_a_later_mention_on_the_same_video() {
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("bell-then-mention-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        // The bell fires first: the watcher is not `p`-tagged on the original.
        let published = EventBuilder::new(Kind::from(KIND_VIDEO), "first version")
            .tag(Tag::identifier("bell-then-mention"))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &published,
            &watcher.public_key(),
            NotificationType::NewPost,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            1,
            "the bell delivers"
        );

        // The creator then edits the video and `p`-tags the watcher. That is a
        // different notification carrying different information, so it must not
        // be eaten by the bell's record for the same coordinate.
        let edit_adding_mention = EventBuilder::new(Kind::from(KIND_VIDEO), "edited version")
            .tag(Tag::identifier("bell-then-mention"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &edit_adding_mention,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            2,
            "a delivered bell must not suppress a later mention on the same video"
        );

        // The per-type record still works within its own type: a second edit
        // does not re-notify the mention.
        let further_edit = EventBuilder::new(Kind::from(KIND_VIDEO), "second edit")
            .tag(Tag::identifier("bell-then-mention"))
            .tag(Tag::public_key(watcher.public_key()))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &further_edit,
            &watcher.public_key(),
            NotificationType::Mention,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            mock_sender.get_sent_messages().len(),
            2,
            "the mention's own record still suppresses a repeat edit"
        );

        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();
        let bell_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::NewPost)
                .unwrap();
        let mention_key =
            video_recipient_claim_key(&published, &watcher.public_key(), NotificationType::Mention)
                .unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("DEL")
            .arg(format!("dedup:{bell_key}"))
            .arg(format!("dedup:{mention_key}"))
            .arg(redis_store::build_notify_rate_key(
                &watcher.public_key(),
                &owner.public_key(),
            ))
            .query_async::<()>(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_a_failed_send_does_not_burn_the_rate_limit_window() {
        // Check-then-set-on-success is a deliberate choice over `SET NX EX`, and
        // this is the behaviour it was chosen for: an FCM blip must not cost the
        // watcher an hour of bells. The comment above the write says so and asks
        // that it not be "fixed" later, which is precisely why it needs a test
        // rather than a comment.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let owner = Keys::generate();
        let watcher = Keys::generate();
        let fcm_token = format!("failed-send-{}", watcher.public_key().to_hex());
        redis_store::add_or_update_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        let mock_sender = MockFcmSender::new();
        // Transient, not `TokenNotRegistered`: the token stays registered so the
        // next attempt can still succeed, which is the case the window protects.
        mock_sender.set_error_for_token(&fcm_token, FcmError::InternalError);
        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(mock_sender.clone())),
        );

        let published = EventBuilder::new(Kind::from(KIND_VIDEO), "a new vine")
            .tag(Tag::identifier("failed-send"))
            .sign_with_keys(&owner)
            .unwrap();
        send_notification_to_user(
            &state,
            &published,
            &watcher.public_key(),
            NotificationType::NewPost,
            &test_copy(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let rate_key =
            redis_store::build_notify_rate_key(&watcher.public_key(), &owner.public_key());
        let marker = redis_store::get_cached_string(&pool, &rate_key)
            .await
            .unwrap();

        // Cleanup before the assertions, so a failure does not leave a
        // registered token behind in the developer's Redis.
        redis_store::remove_token(&pool, &watcher.public_key(), &fcm_token)
            .await
            .unwrap();

        assert!(
            mock_sender.get_sent_messages().is_empty(),
            "the send was supposed to fail"
        );
        assert!(
            marker.is_none(),
            "a bell nobody received must not consume the watcher's hour"
        );
    }

    #[test]
    fn test_find_comment_recipients_reply_notifies_root_and_parent_authors() {
        let actor = Keys::generate();
        let video_owner = Keys::generate();
        let parent_comment_author = Keys::generate();

        // A NIP-22 reply to a comment on a video: the uppercase `P` is the root
        // author (the video owner) and the lowercase `p` is the parent comment's
        // author. Both must be notified, and they are distinct pubkeys here.
        let event = EventBuilder::new(Kind::from(1111), "replying")
            .tag(Tag::parse(["P", video_owner.public_key().to_hex().as_str()]).unwrap())
            .tag(Tag::public_key(parent_comment_author.public_key()))
            .sign_with_keys(&actor)
            .unwrap();

        let recipients = find_comment_recipients(&event);
        assert_eq!(recipients.len(), 2);
        assert!(recipients.contains(&video_owner.public_key()));
        assert!(recipients.contains(&parent_comment_author.public_key()));
    }

    #[test]
    fn test_find_comment_recipients_top_level_dedups_root_and_parent() {
        let actor = Keys::generate();
        let video_owner = Keys::generate();

        // A top-level comment on a video: the parent scope equals the root scope,
        // so uppercase `P` and lowercase `p` both point at the video owner. The
        // owner must be notified exactly once.
        let event = EventBuilder::new(Kind::from(1111), "nice video")
            .tag(Tag::parse(["P", video_owner.public_key().to_hex().as_str()]).unwrap())
            .tag(Tag::public_key(video_owner.public_key()))
            .sign_with_keys(&actor)
            .unwrap();

        let recipients = find_comment_recipients(&event);
        assert_eq!(recipients, vec![video_owner.public_key()]);
    }

    #[test]
    fn test_find_comment_recipients_none_without_author_tags() {
        let actor = Keys::generate();

        // A NIP-22 comment scoped to an external identity (`I`/`i`, e.g. a URL)
        // carries no `P`/`p` author tags, so there is nobody to notify.
        let event = EventBuilder::new(Kind::from(1111), "nice article")
            .tag(Tag::parse(["I", "https://example.com/article"]).unwrap())
            .tag(Tag::parse(["K", "web"]).unwrap())
            .sign_with_keys(&actor)
            .unwrap();

        let recipients = find_comment_recipients(&event);
        assert!(recipients.is_empty());
    }

    // =========================================================================
    // Notification Type Detection Tests
    // =========================================================================

    #[test]
    fn test_comment_vs_mention_detection_comment() {
        let sender = Keys::generate();
        let target = Keys::generate();

        // A reply (has e-tag) should be a Comment
        let reply_event = EventBuilder::text_note("This is a reply")
            .tag(Tag::event(EventId::all_zeros())) // e-tag makes it a reply
            .tag(Tag::public_key(target.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        let has_e_tag = reply_event.tags.find(TagKind::e()).is_some();
        assert!(has_e_tag, "Reply should have e-tag");
    }

    #[test]
    fn test_comment_vs_mention_detection_mention() {
        let sender = Keys::generate();
        let target = Keys::generate();

        // A mention (no e-tag, only p-tag) should be a Mention
        let mention_event = EventBuilder::text_note("Hey @user check this out")
            .tag(Tag::public_key(target.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        let has_e_tag = mention_event.tags.find(TagKind::e()).is_some();
        assert!(!has_e_tag, "Mention should not have e-tag");
    }

    // =========================================================================
    // Service Targeting Tests
    // =========================================================================

    #[test]
    fn test_is_event_for_service_targeted() {
        let sender = Keys::generate();
        let service = Keys::generate();

        let event = EventBuilder::new(Kind::from(KIND_REGISTRATION), "encrypted_content")
            .tag(Tag::public_key(service.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        assert!(is_event_for_service(&event, &service.public_key()));
    }

    #[test]
    fn test_is_event_for_service_not_targeted() {
        let sender = Keys::generate();
        let service = Keys::generate();
        let other_service = Keys::generate();

        // Event targeted to a different service
        let event = EventBuilder::new(Kind::from(KIND_REGISTRATION), "encrypted_content")
            .tag(Tag::public_key(other_service.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        assert!(!is_event_for_service(&event, &service.public_key()));
    }

    #[test]
    fn test_is_event_for_service_no_p_tag() {
        let sender = Keys::generate();
        let service = Keys::generate();

        // Event without p-tag
        let event = EventBuilder::new(Kind::from(KIND_REGISTRATION), "encrypted_content")
            .sign_with_keys(&sender)
            .unwrap();

        assert!(!is_event_for_service(&event, &service.public_key()));
    }

    #[test]
    fn test_is_event_for_service_multiple_p_tags() {
        let sender = Keys::generate();
        let service = Keys::generate();
        let other = Keys::generate();

        // Event with multiple p-tags, one of which is our service
        let event = EventBuilder::new(Kind::from(KIND_REGISTRATION), "encrypted_content")
            .tag(Tag::public_key(other.public_key()))
            .tag(Tag::public_key(service.public_key()))
            .sign_with_keys(&sender)
            .unwrap();

        assert!(is_event_for_service(&event, &service.public_key()));
    }

    // =========================================================================
    // Authoritative Target Field Tests (referenced event id + addressable coord)
    // =========================================================================

    #[test]
    fn test_insert_reference_fields_like_with_addressable_coordinate() {
        let actor = Keys::generate();
        let owner = Keys::generate();
        let video_event_id = "c".repeat(64);
        let address = format!("34236:{}:my-vine-id", owner.public_key().to_hex());

        // A like (kind 7) on a video carries a lowercase `a` coordinate plus the
        // video event id in `e` and the owner in `p`.
        let event = EventBuilder::new(Kind::Reaction, "+")
            .tag(Tag::parse(["e", video_event_id.as_str()]).unwrap())
            .tag(Tag::parse(["a", address.as_str()]).unwrap())
            .tag(Tag::public_key(owner.public_key()))
            .tag(Tag::parse(["k", "34236"]).unwrap())
            .sign_with_keys(&actor)
            .unwrap();

        let mut data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        insert_reference_fields(&mut data, &event);

        assert_eq!(data.get("referencedEventId"), Some(&video_event_id));
        assert_eq!(data.get("referencedAddress"), Some(&address));
        assert_eq!(data.get("referencedKind"), Some(&"34236".to_string()));
        assert_eq!(
            data.get("referencedAuthorPubkey"),
            Some(&owner.public_key().to_hex())
        );
        assert_eq!(data.get("referencedDTag"), Some(&"my-vine-id".to_string()));
    }

    #[test]
    fn test_insert_video_reference_fields_uses_trigger_identity() {
        let owner = Keys::generate();
        let event = EventBuilder::new(Kind::from(34236), "inspired video")
            .tag(Tag::identifier("video:d-tag"))
            .sign_with_keys(&owner)
            .unwrap();
        let address = format!("34236:{}:video:d-tag", owner.public_key().to_hex());

        let mut data = std::collections::HashMap::new();
        insert_trigger_reference_fields(&mut data, &event);

        assert_eq!(data.get("referencedEventId"), Some(&event.id.to_hex()));
        assert_eq!(data.get("referencedAddress"), Some(&address));
        assert_eq!(data.get("referencedKind"), Some(&"34236".to_string()));
        assert_eq!(
            data.get("referencedAuthorPubkey"),
            Some(&owner.public_key().to_hex())
        );
        assert_eq!(data.get("referencedDTag"), Some(&"video:d-tag".to_string()));
    }

    #[test]
    fn test_insert_reference_fields_prefers_nip22_root_scope() {
        let actor = Keys::generate();
        let owner = Keys::generate();
        let root_video_id = "a".repeat(64);
        let parent_comment_id = "b".repeat(64);
        let address = format!("34236:{}:root-vine", owner.public_key().to_hex());

        // A NIP-22 comment (kind 1111): the uppercase root scope (`A`/`E`) is the
        // video; the lowercase parent (`a`/`e`) is the comment being replied to.
        // The authoritative target must be the root video, not the parent comment.
        let event = EventBuilder::new(Kind::from(1111), "nice!")
            .tag(Tag::parse(["E", root_video_id.as_str()]).unwrap())
            .tag(Tag::parse(["A", address.as_str()]).unwrap())
            .tag(Tag::parse(["K", "34236"]).unwrap())
            .tag(Tag::parse(["e", parent_comment_id.as_str()]).unwrap())
            .tag(Tag::parse(["k", "1111"]).unwrap())
            .sign_with_keys(&actor)
            .unwrap();

        let mut data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        insert_reference_fields(&mut data, &event);

        // Anchors to the root video, NOT the parent comment.
        assert_eq!(data.get("referencedEventId"), Some(&root_video_id));
        assert_eq!(data.get("referencedAddress"), Some(&address));
        assert_eq!(
            data.get("referencedAuthorPubkey"),
            Some(&owner.public_key().to_hex())
        );
        assert_eq!(data.get("referencedDTag"), Some(&"root-vine".to_string()));
    }

    #[test]
    fn test_insert_reference_fields_event_id_only_when_no_coordinate() {
        let actor = Keys::generate();
        let comment_id = "d".repeat(64);

        // A like on a comment (kind 1111 target) has an `e` tag but no `a` tag.
        let event = EventBuilder::new(Kind::Reaction, "+")
            .tag(Tag::parse(["e", comment_id.as_str()]).unwrap())
            .tag(Tag::parse(["k", "1111"]).unwrap())
            .sign_with_keys(&actor)
            .unwrap();

        let mut data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        insert_reference_fields(&mut data, &event);

        assert_eq!(data.get("referencedEventId"), Some(&comment_id));
        assert!(!data.contains_key("referencedAddress"));
        assert!(!data.contains_key("referencedDTag"));
        assert!(!data.contains_key("referencedAuthorPubkey"));
        assert!(!data.contains_key("referencedKind"));
    }

    #[test]
    fn test_insert_reference_fields_none_when_no_reference_tags() {
        let actor = Keys::generate();
        let target = Keys::generate();

        // A mention carries only a `p` tag: no addressable target, no event ref.
        let event = EventBuilder::text_note("hey @you")
            .tag(Tag::public_key(target.public_key()))
            .sign_with_keys(&actor)
            .unwrap();

        let mut data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        insert_reference_fields(&mut data, &event);

        assert!(data.is_empty());
    }

    #[test]
    fn test_insert_reference_fields_preserves_colons_in_dtag() {
        let actor = Keys::generate();
        let owner = Keys::generate();
        // A d-tag may itself contain ':'; the split must keep it intact.
        let address = format!("34236:{}:weird:d:tag", owner.public_key().to_hex());

        let event = EventBuilder::new(Kind::Reaction, "+")
            .tag(Tag::parse(["a", address.as_str()]).unwrap())
            .sign_with_keys(&actor)
            .unwrap();

        let mut data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        insert_reference_fields(&mut data, &event);

        assert_eq!(data.get("referencedAddress"), Some(&address));
        assert_eq!(data.get("referencedDTag"), Some(&"weird:d:tag".to_string()));
    }
}
