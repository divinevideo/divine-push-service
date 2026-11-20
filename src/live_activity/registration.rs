//! Handling of a kind 3079 registration that carries a Live Activity.
//!
//! Such a registration never touches device registration (`user_tokens:*`,
//! `token_to_pubkey`): the phone may have declined notifications, and a token
//! stored there would keep receiving pushes. It only starts, updates or ends
//! the activity in the Live Activity store.

use crate::error::Result;
use crate::event_handler::{CONTROL_EVENT_FUTURE_SKEW_SECS, REPLAY_HORIZON_SECS};
use crate::fcm_sender::token_prefix;
use crate::live_activity::payload::{self, ActivityState};
use crate::live_activity::store::{self, EndOutcome, UpsertOutcome};
use crate::metrics;
use crate::state::AppState;
use nostr_sdk::prelude::*;
use serde_json::Value;
use tracing::{debug, info, warn};

/// Apply a Live Activity registration.
///
/// `raw` is the whole decrypted 3079 JSON; its `liveActivity` object is
/// validated against the event's signer. Events outside the accepted time
/// window and invalid objects are dropped with `Ok`. An `ended` registration is
/// always applied, so turning the feature on later never resumes an activity
/// the person ended; an `active` one is stored only while the feature is on.
pub async fn handle(state: &AppState, event: &Event, fcm_token: &str, raw: &Value) -> Result<()> {
    let event_id = event.id.to_hex();
    let owner = event.pubkey.to_hex();
    let now = Timestamp::now().as_secs();
    let at = event.created_at.as_secs();

    if at > now.saturating_add(CONTROL_EVENT_FUTURE_SKEW_SECS)
        || at < now.saturating_sub(REPLAY_HORIZON_SECS)
    {
        warn!(%event_id, pubkey = %owner, created_at = at, "Rejected Live Activity registration outside accepted time window");
        metrics::live_activity_registration("out_of_window");
        return Ok(());
    }

    let Some(object) = raw.get("liveActivity") else {
        warn!(%event_id, pubkey = %owner, reason = "no liveActivity object", "Dropped invalid Live Activity registration");
        metrics::live_activity_registration("invalid");
        return Ok(());
    };
    let reg = match payload::parse(object, &owner, now) {
        Ok(reg) => reg,
        Err(e) => {
            warn!(%event_id, pubkey = %owner, reason = %e, "Dropped invalid Live Activity registration");
            metrics::live_activity_registration("invalid");
            return Ok(());
        }
    };
    let activity_prefix = token_prefix(&reg.activity_token);
    let fcm_prefix = token_prefix(fcm_token);

    if reg.state == ActivityState::Ended {
        let outcome = store::end_by_token(&state.redis_pool, &owner, &reg, at, &event_id).await?;
        let label = match outcome {
            EndOutcome::Ended => "ended",
            EndOutcome::TombstonedUnseen => "tombstoned_unseen",
            EndOutcome::Stale => "stale",
        };
        info!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, outcome = label, "Applied Live Activity end");
        metrics::live_activity_registration(label);
        return Ok(());
    }

    let settings = &state.settings.live_activity;
    if !settings.enabled {
        debug!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, "Live Activities are disabled; not storing the activity");
        metrics::live_activity_registration("disabled");
        return Ok(());
    }
    if reg.window_ends_at <= now {
        debug!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, window_ends_at = reg.window_ends_at, "Ignored Live Activity whose window has passed");
        metrics::live_activity_registration("expired");
        return Ok(());
    }

    let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
    let first_due_ms = now_ms.saturating_add(settings.cadence_secs.saturating_mul(1000));
    let outcome = store::upsert_active(
        &state.redis_pool,
        &owner,
        fcm_token,
        &reg,
        at,
        &event_id,
        first_due_ms,
        settings.max_active_per_owner,
        settings.max_active_total,
    )
    .await?;
    let label = match &outcome {
        UpsertOutcome::Created => "created",
        UpsertOutcome::Updated => "updated",
        UpsertOutcome::Superseded { .. } => "superseded",
        UpsertOutcome::Stale => "stale",
        UpsertOutcome::Tombstoned => "tombstoned",
        UpsertOutcome::OverOwnerCap => "over_owner_cap",
        UpsertOutcome::OverTotalCap => "over_total_cap",
        UpsertOutcome::TokenOwnedElsewhere => "token_owned_elsewhere",
    };
    match &outcome {
        UpsertOutcome::OverOwnerCap | UpsertOutcome::OverTotalCap => {
            warn!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, fcm_token_prefix = %fcm_prefix, outcome = label, "Refused Live Activity over its cap");
        }
        UpsertOutcome::TokenOwnedElsewhere => {
            warn!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, outcome = label, "Refused Live Activity whose token is bound to another pubkey");
        }
        UpsertOutcome::Stale | UpsertOutcome::Tombstoned => {
            debug!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, outcome = label, "Ignored Live Activity registration");
        }
        UpsertOutcome::Superseded { previous_id } => {
            info!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, fcm_token_prefix = %fcm_prefix, %previous_id, outcome = label, "Stored Live Activity");
        }
        UpsertOutcome::Created | UpsertOutcome::Updated => {
            info!(%event_id, pubkey = %owner, activity_token_prefix = %activity_prefix, fcm_token_prefix = %fcm_prefix, outcome = label, "Stored Live Activity");
        }
    }
    metrics::live_activity_registration(label);
    Ok(())
}
