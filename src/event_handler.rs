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

// Replay horizon: ignore events older than this
const REPLAY_HORIZON_DAYS: u64 = 7;

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

                // Check replay horizon - ignore events that are too old
                if is_event_too_old(&event) {
                    debug!(event_id = %event_id, created_at = %event.created_at, "Ignoring old event beyond replay horizon");
                    continue;
                }

                // Atomically claim the event to prevent duplicate processing across replicas
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

    let (notification_type, recipients) = if kind_num == 7 {
        // Kind 7: Reaction/Like - notify the author of the liked event
        let recipients = find_reaction_recipients(event);
        (NotificationType::Like, recipients)
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
        (notification_type, recipients)
    } else if kind_num == 1111 {
        // Kind 1111: NIP-22 comment (diVine publishes video comments here, not
        // as kind 1). Notify the root author (uppercase `P`, the video owner)
        // and the direct parent author (lowercase `p`). create_fcm_payload
        // attaches the authoritative root-video target from the uppercase `A`.
        let recipients = find_comment_recipients(event);
        (NotificationType::Comment, recipients)
    } else if kind_num == 3 {
        // Kind 3: Contact list - notify newly followed users
        // Note: This would require tracking previous contact list state
        // For now, we skip this as it requires state comparison
        debug!(event_id = %event_id, "Contact list event - follow notifications not yet implemented");
        return Ok(());
    } else if kind_num == 16 {
        // Kind 16: Repost - notify the author of the reposted event
        let recipients = find_repost_recipients(event);
        (NotificationType::Repost, recipients)
    } else if kind_num == 30023 {
        // Kind 30023: Long-form content - check for mentions
        let recipients = find_mentioned_pubkeys(event);
        (NotificationType::Mention, recipients)
    } else {
        trace!(event_id = %event_id, kind = %event_kind, "Ignoring event kind - no notification handler");
        return Ok(());
    };

    if recipients.is_empty() {
        debug!(event_id = %event_id, kind = %event_kind, "No recipients found for event");
        return Ok(());
    }

    info!(
        event_id = %event_id,
        kind = %event_kind,
        notification_type = ?notification_type,
        recipient_count = recipients.len(),
        "Processing notification event"
    );

    // Send notifications to each recipient
    for recipient_pubkey in recipients {
        if token.is_cancelled() {
            info!(event_id = %event_id, "Notification sending cancelled");
            return Err(crate::error::ServiceError::Cancelled);
        }

        // Skip if recipient is the sender
        if recipient_pubkey == event.pubkey {
            trace!(event_id = %event_id, "Skipping notification to sender");
            continue;
        }

        if let Err(e) = send_notification_to_user(
            state,
            event,
            &recipient_pubkey,
            notification_type,
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

    if !notification_type.is_enabled(&prefs) {
        info!(
            event_id = %event_id,
            target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
            notification_type = ?notification_type,
            "Notification type disabled by user preferences - skipping"
        );
        return Ok(());
    }

    info!(
        event_id = %event_id,
        target_pubkey = %target_pubkey.to_bech32().unwrap_or_else(|_| "unknown".to_string()),
        token_count = tokens.len(),
        "Found FCM tokens for recipient"
    );

    // Create FCM payload
    let payload = create_fcm_payload(event, target_pubkey, notification_type, state).await?;

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
async fn create_fcm_payload(
    event: &Event,
    target_pubkey: &PublicKey,
    notification_type: NotificationType,
    state: &AppState,
) -> Result<FcmPayload> {
    let mut data = std::collections::HashMap::new();

    // Get sender name using mention parser service
    let sender_name = if let Some(ref mention_parser) = state.mention_parser_service {
        match mention_parser
            .get_display_name(&event.pubkey.to_hex())
            .await
        {
            Ok(Some(name)) => name,
            Ok(None) => format_short_npub(&event.pubkey),
            Err(e) => {
                warn!(error = %e, "Failed to get sender display name");
                format_short_npub(&event.pubkey)
            }
        }
    } else {
        format_short_npub(&event.pubkey)
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
            // Format content with mention parser if available
            let formatted_content = if let Some(ref mention_parser) = state.mention_parser_service {
                match mention_parser.format_content_for_push(&event.content).await {
                    Ok(formatted) => formatted,
                    Err(_) => event.content.clone(),
                }
            } else {
                event.content.clone()
            };
            let body = format!(
                "{}: {}",
                sender_name,
                truncate_string(&formatted_content, 150)
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
            // Format content with mention parser if available
            let formatted_content = if let Some(ref mention_parser) = state.mention_parser_service {
                match mention_parser.format_content_for_push(&event.content).await {
                    Ok(formatted) => formatted,
                    Err(_) => event.content.clone(),
                }
            } else {
                event.content.clone()
            };
            let body = format!(
                "{}: {}",
                sender_name,
                truncate_string(&formatted_content, 150)
            );
            (title, body)
        }
        NotificationType::Repost => {
            let title = "New repost".to_string();
            let body = format!("{} reposted your post", sender_name);
            (title, body)
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
    insert_reference_fields(&mut data, event);

    Ok(FcmPayload {
        notification: None, // Data-only message for better client control
        data: Some(data),
        android: None,
        webpush: None,
        apns: None,
    })
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
    use nostr_sdk::prelude::{Keys, SecretKey};

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
