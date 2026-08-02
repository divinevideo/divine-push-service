//! Event handler for diVine Push Service
//!
//! Handles Nostr events and routes them to appropriate notification handlers.
//! Supports:
//! - Token registration/deregistration (kinds 3079/3080)
//! - Notification types: likes, comments, follows, mentions, reposts

use crate::{
    crypto::CryptoService,
    error::Result,
    fcm_sender,
    models::FcmPayload,
    preferences::{self, NotificationType, UserPreferences},
    redis_store,
    state::AppState,
};
use nostr_sdk::prelude::*;
use std::str::FromStr;
use std::sync::Arc;
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
/// The claim exists to stop two replicas sending the same push twice, and for
/// every handler that sends a push it is the only thing standing between a
/// duplicate and the user. Notify lists send nothing. They build persistent
/// state through `replace_notify_subscriptions`, a single atomic Lua script that
/// already rejects any list not strictly newer than the stored one, so
/// concurrent replicas applying the same list are a no-op with or without the
/// claim.
///
/// Meanwhile the claim is taken *before* routing and never released, so a
/// transient Redis error inside the handler leaves it standing: the historical
/// replay on the next restart skips the event as already-claimed, and that
/// subscriber's bells stay dark until `processed_event_ttl_secs` expires, seven
/// days by default. So the claim trades an outage of a user's subscriptions for
/// deduplication the Lua script performs anyway.
///
/// Scoped by `is_notify_list` rather than kind alone, symmetric with
/// `is_beyond_replay_horizon`: the idempotency argument rests on the Lua script,
/// which only runs for `d=notify`.
pub fn requires_event_claim(event: &Event) -> bool {
    !is_notify_list(event)
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

    // Clean up user preferences when they deregister
    let pubkey_hex = event.pubkey.to_hex();
    if let Err(e) = preferences::delete_user_preferences(&state.redis_pool, &pubkey_hex).await {
        warn!(event_id = %event.id, pubkey = %event.pubkey, error = %e, "Failed to delete user preferences");
    }

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
    } else if kind_num == 3 {
        // Kind 3: Contact list - notify newly followed users
        // Note: This would require tracking previous contact list state
        // For now, we skip this as it requires state comparison
        debug!(event_id = %event_id, "Contact list event - follow notifications not yet implemented");
        return Ok(());
    } else if kind_num == 16 {
        // Kind 16: Repost - notify the author of the reposted event
        targets_of(NotificationType::Repost, find_repost_recipients(event))
    } else if kind_num == 30023 {
        // Kind 30023: Long-form content - check for mentions
        targets_of(NotificationType::Mention, find_mentioned_pubkeys(event))
    } else if kind_num == KIND_VIDEO {
        video_notification_targets(state, event).await
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

    // Send notifications to each target
    for target in targets {
        if token.is_cancelled() {
            info!(event_id = %event_id, "Notification sending cancelled");
            return Err(crate::error::ServiceError::Cancelled);
        }

        let recipient_pubkey = target.recipient;

        if let Err(e) = send_notification_to_user(
            state,
            event,
            &recipient_pubkey,
            target.notification_type,
            &copy,
            token.clone(),
        )
        .await
        {
            if matches!(e, crate::error::ServiceError::Cancelled) {
                return Err(e);
            }
            error!(
                event_id = %event_id,
                recipient = %recipient_pubkey,
                error = %e,
                "Failed to send notification"
            );
        }
    }

    Ok(())
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
/// their own activity. Kept as a pure function, like `merge_watcher_targets`,
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
        NotificationType::Like
        | NotificationType::Follow
        | NotificationType::Repost
        | NotificationType::NewPost => false,
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

    /// Pre-resolved copy, for tests that exercise delivery rather than copy.
    #[cfg(test)]
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

/// Merge bell watchers into a video's mention targets.
///
/// Mention wins on overlap: someone who both watches `author` and is mentioned
/// in the video gets exactly one push, typed `Mention`, because that is the more
/// specific signal. Watchers already present as mention targets are therefore
/// skipped rather than added.
///
/// Kept separate from the Redis read so the dedup rule is testable on its own.
fn merge_watcher_targets(
    mut targets: Vec<NotificationTarget>,
    author: &PublicKey,
    watchers: Vec<PublicKey>,
) -> Vec<NotificationTarget> {
    for watcher in watchers {
        // `deliverable_targets` drops self-targets for every event kind, but
        // filtering here keeps the merge rule complete on its own terms.
        if watcher == *author {
            continue;
        }
        if targets.iter().any(|t| t.recipient == watcher) {
            continue;
        }
        targets.push(NotificationTarget {
            recipient: watcher,
            notification_type: NotificationType::NewPost,
        });
    }
    targets
}

/// Resolve a watcher lookup into the watchers to notify, degrading to none.
///
/// Bells are enrichment layered on top of mentions, and they are the only part
/// of a video's targets that needs Redis. A failed lookup must therefore cost
/// the bells and nothing else. Propagating it would discard the mention
/// notifications the same event produced before this feature existed, from
/// tags that were already in hand.
///
/// Kept separate from the read so the degradation is testable without an
/// `AppState`.
fn watchers_or_degrade(event: &Event, lookup: Result<Vec<PublicKey>>) -> Vec<PublicKey> {
    match lookup {
        Ok(watchers) => watchers,
        Err(e) => {
            error!(
                event_id = %event.id,
                creator = %event.pubkey,
                error = %e,
                "Failed to read notify watchers - delivering mentions without bells"
            );
            Vec::new()
        }
    }
}

/// Build the notification targets for a video event: mentions plus bells.
async fn video_notification_targets(state: &AppState, event: &Event) -> Vec<NotificationTarget> {
    let lookup = redis_store::get_notify_watchers(&state.redis_pool, &event.pubkey).await;
    merge_watcher_targets(
        video_mention_targets(event),
        &event.pubkey,
        watchers_or_degrade(event, lookup),
    )
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
/// quietly cost you mention notifications from them. `merge_watcher_targets`
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
/// This is `merge_watcher_targets`'s "mention wins on overlap" rule extended
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

async fn record_video_claims(
    state: &AppState,
    event: &Event,
    target_pubkey: &PublicKey,
    notification_type: NotificationType,
    log_message: &'static str,
) -> Result<()> {
    if event.kind.as_u16() != KIND_VIDEO {
        return Ok(());
    }

    for satisfied in satisfied_video_claims(notification_type) {
        let Some(claim_key) = video_recipient_claim_key(event, target_pubkey, satisfied) else {
            warn!(
                event_id = %event.id,
                target_pubkey = %target_pubkey,
                "Video notification lacked an addressable d-tag"
            );
            return Ok(());
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

    Ok(())
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
            match redis_store::get_notify_watchers(&state.redis_pool, &event.pubkey).await {
                Ok(watchers) => watchers.contains(target_pubkey),
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
            .await?;
            return Ok(());
        }
    }

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
    let mut success_count = 0;

    for (fcm_token, result) in results {
        if token.is_cancelled() {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while processing FCM results.");
            return Err(crate::error::ServiceError::Cancelled);
        }

        let truncated_token = &fcm_token[..8.min(fcm_token.len())];

        match result {
            Ok(_) => {
                success_count += 1;
                trace!(target_pubkey = %target_pubkey, token_prefix = truncated_token, "Successfully sent notification");
            }
            Err(fcm_sender::FcmError::TokenNotRegistered) => {
                warn!(target_pubkey = %target_pubkey, token_prefix = truncated_token, "Token invalid/unregistered, marking for removal.");
                tokens_to_remove.push(fcm_token);
            }
            Err(e) => {
                error!(
                    target_pubkey = %target_pubkey, token_prefix = truncated_token, error = %e,
                    "FCM send failed for token"
                );
            }
        }
    }

    info!(
        event_id = %event_id,
        target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
        success_count,
        failed_count = tokens_to_remove.len(),
        "FCM notification send summary"
    );

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
        .await?;
    }

    // Remove invalid tokens
    if !tokens_to_remove.is_empty() {
        debug!(event_id = %event_id, target_pubkey = %target_pubkey, count = tokens_to_remove.len(), "Removing invalid tokens");
        for fcm_token_to_remove in tokens_to_remove {
            if token.is_cancelled() {
                info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while removing invalid tokens.");
                return Err(crate::error::ServiceError::Cancelled);
            }
            let truncated_token = &fcm_token_to_remove[..8.min(fcm_token_to_remove.len())];
            if let Err(e) =
                redis_store::remove_token(&state.redis_pool, target_pubkey, &fcm_token_to_remove)
                    .await
            {
                error!(
                    target_pubkey = %target_pubkey, token_prefix = truncated_token, error = %e,
                    "Failed to remove invalid token"
                );
            } else {
                info!(target_pubkey = %target_pubkey, token_prefix = truncated_token, "Removed invalid token");
            }
        }
    }

    Ok(())
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
        NotificationType::Follow => {
            let title = "New follower".to_string();
            let body = format!("{} started following you", sender_name);
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
    fn test_watcher_yields_a_new_post_target() {
        let author = Keys::generate().public_key();
        let watcher = Keys::generate().public_key();

        let targets = merge_watcher_targets(Vec::new(), &author, vec![watcher]);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].recipient, watcher);
        assert_eq!(targets[0].notification_type, NotificationType::NewPost);
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
        assert!(!renders_event_content(NotificationType::Follow));
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

    #[test]
    fn test_mentioned_watcher_yields_exactly_one_mention_target() {
        let author = Keys::generate().public_key();
        let both = Keys::generate().public_key();

        let mentions = vec![NotificationTarget {
            recipient: both,
            notification_type: NotificationType::Mention,
        }];
        let targets = merge_watcher_targets(mentions, &author, vec![both]);

        assert_eq!(targets.len(), 1, "mention wins, so no second push");
        assert_eq!(targets[0].notification_type, NotificationType::Mention);
    }

    #[test]
    fn test_author_watching_themselves_yields_nothing() {
        let author = Keys::generate().public_key();

        let targets = merge_watcher_targets(Vec::new(), &author, vec![author]);

        assert!(targets.is_empty());
    }

    #[test]
    fn test_no_watchers_leaves_mention_targets_unchanged() {
        let author = Keys::generate().public_key();
        let mentioned = Keys::generate().public_key();

        let mentions = vec![NotificationTarget {
            recipient: mentioned,
            notification_type: NotificationType::Mention,
        }];
        let targets = merge_watcher_targets(mentions, &author, Vec::new());

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].recipient, mentioned);
        assert_eq!(targets[0].notification_type, NotificationType::Mention);
    }

    #[test]
    fn test_failed_watcher_lookup_still_delivers_mentions() {
        let author = Keys::generate();
        let mentioned = Keys::generate().public_key();
        let event = EventBuilder::new(Kind::from(KIND_VIDEO), "video")
            .tag(Tag::identifier("vid-1"))
            .tag(Tag::public_key(mentioned))
            .sign_with_keys(&author)
            .unwrap();

        // Redis is down. The bells are lost; the mentions must not be, because
        // they came from the event's own tags and needed no lookup at all.
        let lookup = Err(crate::error::ServiceError::Internal(
            "redis down".to_string(),
        ));
        let watchers = watchers_or_degrade(&event, lookup);
        assert!(watchers.is_empty());

        let targets = merge_watcher_targets(
            video_mention_targets(&event),
            &author.public_key(),
            watchers,
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].recipient, mentioned);
        assert_eq!(targets[0].notification_type, NotificationType::Mention);
    }

    #[tokio::test]
    async fn test_a_real_watcher_lookup_failure_still_delivers_mentions() {
        // The test above proves `watchers_or_degrade` degrades. It does not
        // prove anything calls it: it never reaches `video_notification_targets`,
        // where the degradation is actually wired in, so replacing that call
        // with `lookup.expect(...)` leaves the whole suite green. This drives a
        // genuine `get_notify_watchers` failure through the real function, by
        // leaving a string where the watcher set belongs so `SMEMBERS` fails
        // with WRONGTYPE.
        let Some(pool) = test_redis_pool().await else {
            return;
        };
        let author = Keys::generate();
        let mentioned = Keys::generate().public_key();
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
        let lookup_failed = redis_store::get_notify_watchers(&pool, &author.public_key())
            .await
            .is_err();

        let state = test_app_state(
            crate::config::Settings::new().unwrap(),
            pool.clone(),
            FcmClient::new_with_impl(Box::new(MockFcmSender::new())),
        );
        let targets = video_notification_targets(&state, &event).await;

        let _: () = redis::cmd("DEL")
            .arg(&watchers_key)
            .query_async(&mut *conn)
            .await
            .unwrap();

        assert!(lookup_failed, "the seeded key must make the lookup fail");
        assert_eq!(targets.len(), 1, "the mention survives the failed lookup");
        assert_eq!(targets[0].recipient, mentioned);
        assert_eq!(targets[0].notification_type, NotificationType::Mention);
    }

    #[test]
    fn test_watcher_and_separate_mention_both_get_their_own_type() {
        let author = Keys::generate().public_key();
        let mentioned = Keys::generate().public_key();
        let watcher = Keys::generate().public_key();

        let mentions = vec![NotificationTarget {
            recipient: mentioned,
            notification_type: NotificationType::Mention,
        }];
        let targets = merge_watcher_targets(mentions, &author, vec![watcher]);

        assert_eq!(targets.len(), 2);
        let mention = targets.iter().find(|t| t.recipient == mentioned).unwrap();
        let bell = targets.iter().find(|t| t.recipient == watcher).unwrap();
        assert_eq!(mention.notification_type, NotificationType::Mention);
        assert_eq!(bell.notification_type, NotificationType::NewPost);
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
        // The other direction of the same rule. `merge_watcher_targets` already
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
