//! One Live Activity's cycle: decide what to show, send it through Firebase,
//! then reschedule or finish the activity.
//!
//! The worker loop claims due activities, fetches their stats and any `/views`
//! fallback, and hands each one to [`process_one`]. Nothing here touches device
//! registration (`user_tokens:*`, `token_to_pubkey`): a dead token finishes the
//! activity only.

use std::time::Duration;

use tracing::{debug, info, warn};

use crate::fcm_sender::{
    live_activity_send_outcome, token_prefix, FcmError, LiveActivityEvent, LiveActivityMessage,
    SendOutcome,
};
use crate::live_activity::content::{decide, outage_end, ContentState, Decision};
use crate::live_activity::stats_client::{BulkStats, StatsError};
use crate::live_activity::store::{self, StoredActivity};
use crate::metrics;
use crate::state::AppState;

/// Seconds after a send that iOS marks the activity stale and APNs drops the push.
pub const STALE_AFTER_SECS: u64 = 600;
/// Seconds after an end that iOS removes the activity from the Lock Screen.
pub const DISMISS_AFTER_SECS: u64 = 1800;
/// Seconds past `window_ends_at` that failing stats are waited out before the
/// activity is ended on its last content.
pub const OUTAGE_GRACE_SECS: u64 = 600;
/// A skipped activity is retried at cadence ± this many seconds, so activities
/// that failed together do not all come due together.
const SKIP_JITTER_SECS: u64 = 15;

/// The `/views` fallback result for one activity, gathered by the worker loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewsLookup {
    /// No fallback applies: the bulk stats had views, or the bulk request failed.
    NotNeeded,
    /// The fallback answered.
    Found(Option<u64>),
    /// The fallback failed; treated like a failed stats request.
    Failed,
    /// The per-replica lookup budget is spent; the last views sent are reused.
    BudgetSpent,
}

/// What happened to one activity this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// An update was sent and the next one scheduled.
    Sent,
    /// An end was sent and the activity finished.
    Ended,
    /// Nothing was sent; the activity is retried next cycle.
    Skipped,
    /// The activity was finished without an end send (dead token, rejected
    /// payload, or no longer live).
    Finished,
    /// A send failed and is retried after the given delay.
    Deferred(Duration),
}

/// Runs one claimed activity's cycle: decide, send, then reschedule or finish.
///
/// `stats` is the bulk lookup for this tick. A rate-limited lookup is expected to
/// be handled by the loop before this is called; any error that reaches here is
/// a skip. Store failures are logged and never propagate: the claim's lease
/// expires and the activity is claimed again.
pub async fn process_one(
    state: &AppState,
    activity: &StoredActivity,
    stats: Result<&BulkStats, &StatsError>,
    views: ViewsLookup,
    now_unix: u64,
) -> ProcessOutcome {
    let views_fallback = match views {
        ViewsLookup::NotNeeded => Ok(None),
        ViewsLookup::Found(v) => Ok(v),
        ViewsLookup::Failed => Err(()),
        ViewsLookup::BudgetSpent => Ok(last_views(activity)),
    };
    let skip_reason = if stats.is_err() {
        "stats_failed"
    } else {
        "views_failed"
    };
    let (decision, absent_checks) = decide(
        activity,
        stats.map_err(|_| ()),
        views_fallback,
        activity.absent_checks,
        now_unix,
    );
    // The count reflects stats checks, not sends, so it is kept even if the send fails.
    if absent_checks != activity.absent_checks {
        if let Err(e) =
            store::set_absent_checks(&state.redis_pool, &activity.id, absent_checks).await
        {
            warn!(activity_id = %activity.id, error = %e, "failed to store live activity absent checks");
        }
    }

    match decision {
        Decision::Update(content) => send_update(state, activity, &content, now_unix).await,
        Decision::End(content) => {
            let dismissal = now_unix.saturating_add(DISMISS_AFTER_SECS);
            send_end(state, activity, &content, dismissal, now_unix).await
        }
        Decision::Skip => {
            let window_ends_at = activity.registration.window_ends_at;
            if now_unix >= window_ends_at.saturating_add(OUTAGE_GRACE_SECS) {
                info!(
                    activity_id = %activity.id,
                    owner = %activity.owner,
                    reason = skip_reason,
                    "stats unreachable past the window; ending live activity on its last content"
                );
                let content = outage_end(activity, activity.last_content.as_ref(), now_unix);
                // Removed at once: frozen numbers should not linger on the Lock Screen.
                return send_end(
                    state,
                    activity,
                    &content,
                    now_unix.saturating_sub(1),
                    now_unix,
                )
                .await;
            }
            metrics::live_activity_skipped(skip_reason);
            debug!(activity_id = %activity.id, reason = skip_reason, "skipping live activity this cycle");
            let due = skip_due_unix(&activity.id, now_unix, cadence_secs(state));
            let due_ms = cap_due_ms(activity, now_unix, due.saturating_mul(1000));
            schedule(state, activity, due_ms).await;
            ProcessOutcome::Skipped
        }
    }
}

fn cadence_secs(state: &AppState) -> u64 {
    state.settings.live_activity.cadence_secs
}

/// The views value of the last content sent, or `None` if nothing was sent.
fn last_views(activity: &StoredActivity) -> Option<u64> {
    activity
        .last_content
        .as_ref()
        .and_then(|c| c.get("views"))
        .and_then(serde_json::Value::as_u64)
}

/// Caps a retry or skip so the activity runs again by `window_ends_at` +
/// [`OUTAGE_GRACE_SECS`]. That run sends its end (a summary, or the outage end),
/// well before the hash expires at `window_ends_at` + 2100 s. Once that point
/// has passed the due time is left alone: every caller already keeps it within
/// one cadence, and pinning it to a past instant would retry a failing end on
/// every poll.
fn cap_due_ms(activity: &StoredActivity, now_unix: u64, due_ms: u64) -> u64 {
    let latest = activity
        .registration
        .window_ends_at
        .saturating_add(OUTAGE_GRACE_SECS);
    if now_unix < latest {
        due_ms.min(latest.saturating_mul(1000))
    } else {
        due_ms
    }
}

/// `now + cadence ± 15 s`, never sooner than the next second. The offset is
/// derived from the id and the time, so it varies per activity and per cycle.
fn skip_due_unix(id: &str, now_unix: u64, cadence: u64) -> u64 {
    let hash = blake3::hash(format!("{id}:{now_unix}").as_bytes());
    let offset = u64::from(hash.as_bytes()[0]) % (2 * SKIP_JITTER_SECS + 1);
    now_unix
        .saturating_add(cadence)
        .saturating_add(offset)
        .saturating_sub(SKIP_JITTER_SECS)
        .max(now_unix.saturating_add(1))
}

async fn send_update(
    state: &AppState,
    activity: &StoredActivity,
    content: &ContentState,
    now_unix: u64,
) -> ProcessOutcome {
    if let Some(outcome) = unless_live(state, activity).await {
        return outcome;
    }
    match send(
        state,
        activity,
        LiveActivityEvent::Update,
        content,
        now_unix,
    )
    .await
    {
        Ok(content_state) => {
            metrics::live_activity_send("update", "ok");
            if let Err(e) = store::record_sent(
                &state.redis_pool,
                &activity.id,
                &content_state.to_string(),
                now_unix,
            )
            .await
            {
                warn!(activity_id = %activity.id, error = %e, "failed to record live activity send");
            }
            let next = now_unix
                .saturating_add(cadence_secs(state))
                .min(activity.registration.window_ends_at);
            schedule(state, activity, next.saturating_mul(1000)).await;
            ProcessOutcome::Sent
        }
        Err(e) => after_send_error(state, activity, "update", &e, now_unix).await,
    }
}

async fn send_end(
    state: &AppState,
    activity: &StoredActivity,
    content: &ContentState,
    dismissal_unix: u64,
    now_unix: u64,
) -> ProcessOutcome {
    if let Some(outcome) = unless_live(state, activity).await {
        return outcome;
    }
    let event = LiveActivityEvent::End { dismissal_unix };
    match send(state, activity, event, content, now_unix).await {
        Ok(_) => {
            metrics::live_activity_send("end", "ok");
            info!(
                activity_id = %activity.id,
                owner = %activity.owner,
                post_event_id = %content.post_event_id,
                phase = ?content.phase,
                "ended live activity"
            );
            finish(state, activity).await;
            ProcessOutcome::Ended
        }
        Err(e) => after_send_error(state, activity, "end", &e, now_unix).await,
    }
}

/// Checks the activity is still live right before a send. Returns the outcome
/// to stop with when it is not: a superseded, ended or finished activity is
/// never sent anything.
async fn unless_live(state: &AppState, activity: &StoredActivity) -> Option<ProcessOutcome> {
    match store::is_live(&state.redis_pool, &activity.id).await {
        Ok(true) => None,
        Ok(false) => {
            debug!(activity_id = %activity.id, "live activity no longer live; not sending");
            // Drops any schedule entry left by the claim's lease.
            finish(state, activity).await;
            Some(ProcessOutcome::Finished)
        }
        Err(e) => {
            warn!(activity_id = %activity.id, error = %e, "could not check live activity before sending");
            metrics::live_activity_skipped("store_failed");
            Some(ProcessOutcome::Skipped)
        }
    }
}

/// Sends one event. Returns the content state as sent, for `record_sent`.
async fn send(
    state: &AppState,
    activity: &StoredActivity,
    event: LiveActivityEvent,
    content: &ContentState,
    now_unix: u64,
) -> Result<serde_json::Value, FcmError> {
    // A content state that cannot serialize never will: a rejected payload.
    let content_state =
        serde_json::to_value(content).map_err(|e| FcmError::InvalidRequest(e.to_string()))?;
    let message = LiveActivityMessage {
        fcm_token: activity.fcm_token.clone(),
        activity_token: activity.registration.activity_token.clone(),
        event,
        content_state,
        timestamp_unix: now_unix,
        stale_unix: now_unix.saturating_add(STALE_AFTER_SECS),
    };
    state.fcm_client.send_live_activity(&message).await?;
    Ok(message.content_state)
}

async fn after_send_error(
    state: &AppState,
    activity: &StoredActivity,
    event: &'static str,
    error: &FcmError,
    now_unix: u64,
) -> ProcessOutcome {
    metrics::live_activity_send(event, error.metric_reason());
    let outcome = live_activity_send_outcome(error);
    warn!(
        activity_id = %activity.id,
        owner = %activity.owner,
        fcm_token = %token_prefix(&activity.fcm_token),
        activity_token = %token_prefix(&activity.registration.activity_token),
        event,
        error = %error,
        next = ?outcome,
        "live activity send failed"
    );
    let cadence = Duration::from_secs(cadence_secs(state));
    let delay = match outcome {
        SendOutcome::Finish => {
            finish(state, activity).await;
            return ProcessOutcome::Finished;
        }
        // Firebase's delay is not trusted past one cadence.
        SendOutcome::RetryAfter(delay) => delay.min(cadence),
        SendOutcome::RetryNextCycle => cadence,
    };
    let now_ms = now_unix.saturating_mul(1000);
    let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    let due_ms = cap_due_ms(activity, now_unix, now_ms.saturating_add(delay_ms));
    schedule(state, activity, due_ms).await;
    ProcessOutcome::Deferred(Duration::from_millis(due_ms.saturating_sub(now_ms)))
}

async fn schedule(state: &AppState, activity: &StoredActivity, due_ms: u64) {
    match store::reschedule(&state.redis_pool, &activity.id, due_ms).await {
        Ok(true) => {}
        Ok(false) => {
            debug!(activity_id = %activity.id, "live activity no longer live; not rescheduled")
        }
        Err(e) => {
            warn!(activity_id = %activity.id, error = %e, "failed to reschedule live activity")
        }
    }
}

async fn finish(state: &AppState, activity: &StoredActivity) {
    if let Err(e) = store::finish(&state.redis_pool, &activity.id).await {
        warn!(activity_id = %activity.id, error = %e, "failed to finish live activity");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fcm_sender::{FcmClient, FcmError, LiveActivityEvent, MockFcmSender};
    use crate::live_activity::content::{ContentState, Phase};
    use crate::live_activity::payload::{ActivityState, LiveActivityRegistration};
    use crate::live_activity::stats_client::VersionStats;
    use crate::live_activity::store;
    use crate::redis_store::{self, RedisPool};
    use nostr_sdk::{Keys, PublicKey};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// First due time of a seeded activity: far in the future, so a claim in another
    /// test never picks it up before `process_one` runs. `process_one` then
    /// reschedules it at a real due time, so every test ends with [`Fixture::done`],
    /// which finishes what it seeded and leaves nothing due in the shared Redis.
    const FAR_DUE_MS: u64 = 4_000_000_000_000_000;

    async fn test_pool() -> Option<RedisPool> {
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
        let pool = redis_store::create_pool(&redis_url, 5).await.ok()?;
        let mut conn = pool.get().await.ok()?;
        let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut *conn).await;
        drop(conn);
        pong.ok().map(|_| pool)
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn hex(n: usize) -> String {
        let mut s = String::new();
        while s.len() < n {
            s.push_str(&uuid::Uuid::new_v4().simple().to_string());
        }
        s.truncate(n);
        s
    }

    /// One test's world: its own owner, device, activity and fake Firebase.
    struct Fixture {
        state: AppState,
        mock: MockFcmSender,
        owner: String,
        fcm_token: String,
        event_id: String,
        seeded: std::sync::Mutex<Vec<String>>,
    }

    impl Fixture {
        async fn new() -> Option<Self> {
            let pool = test_pool().await?;
            let mock = MockFcmSender::new();
            let state = AppState {
                settings: crate::config::Settings::new().expect("runtime settings load"),
                redis_pool: pool,
                fcm_client: Arc::new(FcmClient::new_with_impl(Box::new(mock.clone()))),
                service_keys: None,
                crypto_service: None,
                nostr_client: Arc::new(nostr_sdk::Client::default()),
                profile_client: Arc::new(nostr_sdk::Client::default()),
                mention_parser_service: None,
            };
            Some(Self {
                state,
                mock,
                owner: Keys::generate().public_key().to_hex(),
                fcm_token: format!("fcm-{}", hex(40)),
                event_id: hex(64),
                seeded: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// Finishes every activity this test seeded, so none is left scheduled.
        async fn done(&self) {
            let ids = self.seeded.lock().unwrap().clone();
            for id in ids {
                store::finish(self.pool(), &id).await.unwrap();
            }
        }

        fn pool(&self) -> &RedisPool {
            &self.state.redis_pool
        }

        fn cadence(&self) -> u64 {
            self.state.settings.live_activity.cadence_secs
        }

        fn registration(&self, posted_at: u64, window_ends_at: u64) -> LiveActivityRegistration {
            LiveActivityRegistration {
                activity_token: hex(64),
                post_coordinate: format!("34236:{}:{}", self.owner, hex(8)),
                post_event_ids: vec![self.event_id.clone()],
                posted_at,
                window_ends_at,
                show_first_run_note: true,
                state: ActivityState::Active,
            }
        }

        /// Stores `reg` and returns the activity as a claim would hand it over.
        /// `first_seen_at` mirrors `posted_at`, so each test sets the settle point.
        async fn seed(&self, reg: LiveActivityRegistration, event_at: u64) -> StoredActivity {
            let outcome = store::upsert_active(
                self.pool(),
                &self.owner,
                &self.fcm_token,
                &reg,
                event_at,
                &hex(64),
                FAR_DUE_MS,
                5,
                100_000,
            )
            .await
            .unwrap();
            assert!(
                matches!(
                    outcome,
                    store::UpsertOutcome::Created | store::UpsertOutcome::Superseded { .. }
                ),
                "seed failed: {outcome:?}"
            );
            let id = store::activity_id(&self.owner, &reg.activity_token);
            self.seeded.lock().unwrap().push(id.clone());
            StoredActivity {
                id,
                owner: self.owner.clone(),
                fcm_token: self.fcm_token.clone(),
                first_seen_at: reg.posted_at,
                absent_checks: 0,
                last_content: None,
                registration: reg,
            }
        }

        async fn hget(&self, id: &str, field: &str) -> Option<String> {
            let mut conn = self.pool().get().await.unwrap();
            redis::cmd("HGET")
                .arg(format!("live_activity:{id}"))
                .arg(field)
                .query_async(&mut *conn)
                .await
                .unwrap()
        }

        async fn due_ms(&self, id: &str) -> Option<u64> {
            let mut conn = self.pool().get().await.unwrap();
            redis::cmd("ZSCORE")
                .arg("live_activity_jobs")
                .arg(id)
                .query_async(&mut *conn)
                .await
                .unwrap()
        }

        async fn live(&self, id: &str) -> bool {
            store::is_live(self.pool(), id).await.unwrap()
        }

        fn stats(&self, views: Option<u64>, likes: u64, comments: u64) -> BulkStats {
            let mut b = BulkStats::default();
            b.stats.insert(
                self.event_id.clone(),
                VersionStats {
                    views,
                    reactions: likes,
                    comments,
                },
            );
            b
        }

        fn missing(&self) -> BulkStats {
            let mut b = BulkStats::default();
            b.missing.insert(self.event_id.clone());
            b
        }
    }

    fn content(m: &crate::fcm_sender::LiveActivityMessage) -> ContentState {
        serde_json::from_value(m.content_state.clone()).unwrap()
    }

    #[tokio::test]
    async fn tick_sends_update_and_reschedules() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 900, now + 2700);
        let activity = f.seed(reg.clone(), now - 900).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Sent);
        let sent = f.mock.get_live_activity_messages();
        assert_eq!(sent.len(), 1);
        let m = &sent[0];
        assert_eq!(m.event, LiveActivityEvent::Update);
        assert_eq!(m.fcm_token, f.fcm_token);
        assert_eq!(m.activity_token, reg.activity_token);
        assert_eq!(m.timestamp_unix, now);
        assert_eq!(m.stale_unix, now + 600);
        let expected = ContentState {
            phase: Phase::Live,
            views: Some(5),
            likes: Some(2),
            comments: Some(1),
            updated_at: now,
            show_first_run_note: true,
            post_coordinate: reg.post_coordinate.clone(),
            post_event_id: f.event_id.clone(),
        };
        assert_eq!(content(m), expected);
        assert_eq!(
            f.due_ms(&activity.id).await,
            Some((now + f.cadence()) * 1000)
        );
        let last: serde_json::Value =
            serde_json::from_str(&f.hget(&activity.id, "last_content").await.unwrap()).unwrap();
        assert_eq!(last, serde_json::to_value(&expected).unwrap());
        assert_eq!(
            f.hget(&activity.id, "last_sent_at").await,
            Some(now.to_string())
        );
        f.done().await;
    }

    #[tokio::test]
    async fn update_near_window_end_reschedules_at_window_end() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let activity = f.seed(f.registration(now - 3540, now + 60), now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Sent);
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 60) * 1000));
        f.done().await;
    }

    #[tokio::test]
    async fn window_end_sends_summary_end_and_finishes() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let activity = f.seed(f.registration(now - 3600, now), now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(40), 6, 2)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Ended);
        let sent = f.mock.get_live_activity_messages();
        assert_eq!(sent.len(), 1);
        let m = &sent[0];
        assert_eq!(
            m.event,
            LiveActivityEvent::End {
                dismissal_unix: now + 1800
            }
        );
        assert_eq!(m.stale_unix, now + 600);
        let c = content(m);
        assert_eq!(c.phase, Phase::Summary);
        assert_eq!((c.views, c.likes, c.comments), (Some(40), Some(6), Some(2)));
        assert!(!f.live(&activity.id).await);
        assert_eq!(f.due_ms(&activity.id).await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn not_found_needs_two_consecutive_misses() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let mut activity = f.seed(f.registration(now - 900, now + 2700), now).await;

        // First miss: counted, but the activity keeps showing as live.
        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.missing()),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;
        assert_eq!(outcome, ProcessOutcome::Sent);
        assert_eq!(
            content(&f.mock.get_live_activity_messages()[0]).phase,
            Phase::Live
        );
        assert_eq!(
            f.hget(&activity.id, "absent_checks").await.as_deref(),
            Some("1")
        );

        // A failed request in between is neither absence nor presence.
        activity.absent_checks = 1;
        let failed = StatsError::Status(500);
        let outcome = process_one(
            &f.state,
            &activity,
            Err(&failed),
            ViewsLookup::NotNeeded,
            now + 180,
        )
        .await;
        assert_eq!(outcome, ProcessOutcome::Skipped);
        assert_eq!(
            f.hget(&activity.id, "absent_checks").await.as_deref(),
            Some("1")
        );
        assert_eq!(f.mock.get_live_activity_messages().len(), 1);

        // Second consecutive successful miss ends it.
        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.missing()),
            ViewsLookup::NotNeeded,
            now + 360,
        )
        .await;
        assert_eq!(outcome, ProcessOutcome::Ended);
        let sent = f.mock.get_live_activity_messages();
        assert_eq!(sent.len(), 2);
        assert_eq!(content(&sent[1]).phase, Phase::NotFound);
        assert!(!f.live(&activity.id).await);
        f.done().await;
    }

    #[tokio::test]
    async fn not_found_sends_end_with_dismissal() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let mut activity = f.seed(f.registration(now - 900, now + 2700), now).await;
        store::set_absent_checks(f.pool(), &activity.id, 1)
            .await
            .unwrap();
        activity.absent_checks = 1;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&BulkStats::default()),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Ended);
        let sent = f.mock.get_live_activity_messages();
        assert_eq!(sent.len(), 1);
        let m = &sent[0];
        assert_eq!(
            m.event,
            LiveActivityEvent::End {
                dismissal_unix: now + 1800
            }
        );
        assert_eq!(m.stale_unix, now + 600);
        let c = content(m);
        assert_eq!(c.phase, Phase::NotFound);
        assert_eq!((c.views, c.likes, c.comments), (None, None, None));
        assert_eq!(c.post_event_id, f.event_id);
        assert!(!f.live(&activity.id).await);
        assert_eq!(f.due_ms(&activity.id).await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn token_not_registered_finishes() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 900, now + 2700);
        f.mock
            .set_error_for_token(&reg.activity_token, FcmError::TokenNotRegistered);
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Finished);
        assert!(!f.live(&activity.id).await);
        assert_eq!(f.due_ms(&activity.id).await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn token_not_registered_never_prunes_registration() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let pubkey = PublicKey::from_hex(&f.owner).unwrap();
        redis_store::add_or_update_token(f.pool(), &pubkey, &f.fcm_token)
            .await
            .unwrap();
        let token_owner = || async {
            let mut conn = f.pool().get().await.unwrap();
            redis::cmd("HGET")
                .arg("token_to_pubkey")
                .arg(&f.fcm_token)
                .query_async::<Option<String>>(&mut *conn)
                .await
                .unwrap()
        };
        let owner_before = token_owner().await;
        assert!(owner_before.is_some());

        let now = now_secs();
        let reg = f.registration(now - 900, now + 2700);
        f.mock
            .set_error_for_token(&reg.activity_token, FcmError::TokenNotRegistered);
        let activity = f.seed(reg, now).await;
        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Finished);
        assert_eq!(
            redis_store::get_tokens_for_pubkey(f.pool(), &pubkey)
                .await
                .unwrap(),
            vec![f.fcm_token.clone()]
        );
        assert_eq!(token_owner().await, owner_before);
        redis_store::remove_token(f.pool(), &pubkey, &f.fcm_token)
            .await
            .unwrap();
        f.done().await;
    }

    #[tokio::test]
    async fn superseded_activity_is_never_sent() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        // Snapshots of the first activity, as two in-flight claims would hold it:
        // one due an update, one due its window-end summary.
        let updating = f.seed(f.registration(now - 900, now + 2700), now).await;
        let ending = StoredActivity {
            registration: LiveActivityRegistration {
                window_ends_at: now,
                ..updating.registration.clone()
            },
            ..updating.clone()
        };
        // A newer activity on the same device supersedes it.
        f.seed(f.registration(now - 60, now + 3540), now + 1).await;
        assert!(!f.live(&updating.id).await);

        for snapshot in [&updating, &ending] {
            let outcome = process_one(
                &f.state,
                snapshot,
                Ok(&f.stats(Some(5), 2, 1)),
                ViewsLookup::NotNeeded,
                now,
            )
            .await;
            assert_eq!(outcome, ProcessOutcome::Finished);
        }
        assert!(f.mock.get_live_activity_messages().is_empty());
        assert_eq!(f.due_ms(&updating.id).await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn outage_at_window_end_ends_with_last_content_and_immediate_dismissal() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let mut activity = f.seed(f.registration(now - 4200, now - 600), now).await;
        let last = ContentState {
            phase: Phase::Live,
            views: Some(7),
            likes: Some(1),
            comments: None,
            updated_at: now - 700,
            show_first_run_note: true,
            post_coordinate: activity.registration.post_coordinate.clone(),
            post_event_id: f.event_id.clone(),
        };
        let last_json = serde_json::to_value(&last).unwrap();
        store::record_sent(f.pool(), &activity.id, &last_json.to_string(), now - 700)
            .await
            .unwrap();
        activity.last_content = Some(last_json.clone());
        let failed = StatsError::Request("timeout".into());

        let outcome = process_one(
            &f.state,
            &activity,
            Err(&failed),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Ended);
        let sent = f.mock.get_live_activity_messages();
        assert_eq!(sent.len(), 1);
        let m = &sent[0];
        assert_eq!(
            m.event,
            LiveActivityEvent::End {
                dismissal_unix: now - 1
            }
        );
        assert_eq!(m.content_state, last_json);
        assert_eq!(m.stale_unix, now + 600);
        assert!(!f.live(&activity.id).await);
        assert_eq!(f.due_ms(&activity.id).await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn stats_failure_before_outage_grace_skips_until_the_grace_ends() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        // One second short of window end + 600 s: still a skip, not an outage end.
        let activity = f.seed(f.registration(now - 4199, now - 599), now).await;
        let failed = StatsError::Status(503);

        let outcome = process_one(
            &f.state,
            &activity,
            Err(&failed),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Skipped);
        assert!(f.mock.get_live_activity_messages().is_empty());
        // Retried at the grace boundary, not a cadence later, so the outage end
        // goes out while the hash still lives.
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 1) * 1000));
        assert!(f.live(&activity.id).await);
        f.done().await;
    }

    #[tokio::test]
    async fn views_budget_spent_reuses_last_views() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let mut activity = f.seed(f.registration(now - 900, now + 2700), now).await;
        activity.last_content = Some(json!({
            "phase": "live", "views": 42, "likes": 1, "comments": 0,
            "updatedAt": now - 180, "showFirstRunNote": true,
            "postCoordinate": activity.registration.post_coordinate,
            "postEventId": f.event_id,
        }));
        // Every present id reports null views, so the fallback applies.
        let stats = f.stats(None, 3, 2);

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&stats),
            ViewsLookup::BudgetSpent,
            now,
        )
        .await;
        assert_eq!(outcome, ProcessOutcome::Sent);
        let c = content(&f.mock.get_live_activity_messages()[0]);
        assert_eq!((c.views, c.likes, c.comments), (Some(42), Some(3), Some(2)));

        // Nothing sent before: the views stay unknown.
        activity.last_content = None;
        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&stats),
            ViewsLookup::BudgetSpent,
            now + 180,
        )
        .await;
        assert_eq!(outcome, ProcessOutcome::Sent);
        let c = content(&f.mock.get_live_activity_messages()[1]);
        assert_eq!((c.views, c.likes), (None, Some(3)));
        f.done().await;
    }

    #[tokio::test]
    async fn failed_views_lookup_skips() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let activity = f.seed(f.registration(now - 900, now + 2700), now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(None, 3, 2)),
            ViewsLookup::Failed,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Skipped);
        assert!(f.mock.get_live_activity_messages().is_empty());
        let due = f.due_ms(&activity.id).await.unwrap();
        let base = (now + f.cadence()) * 1000;
        assert!(
            (base - 15_000..=base + 15_000).contains(&due),
            "due {due} outside cadence ± 15 s of {base}"
        );
        f.done().await;
    }

    #[tokio::test]
    async fn retryable_send_error_defers() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 900, now + 2700);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::RetryableInternal(Duration::from_secs(30)),
        );
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Deferred(Duration::from_secs(30)));
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 30) * 1000));
        assert!(f.live(&activity.id).await);
        assert_eq!(f.hget(&activity.id, "last_content").await, None);
        f.done().await;
    }
    #[tokio::test]
    async fn huge_retry_after_is_capped_at_cadence() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 900, now + 2700);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::RetryableInternal(Duration::from_secs(10_000)),
        );
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        let cadence = Duration::from_secs(f.cadence());
        assert_eq!(outcome, ProcessOutcome::Deferred(cadence));
        assert_eq!(
            f.due_ms(&activity.id).await,
            Some((now + f.cadence()) * 1000)
        );
        assert!(f.live(&activity.id).await);
        f.done().await;
    }

    #[tokio::test]
    async fn retry_after_never_passes_the_outage_grace() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        // The window closed 500 s ago: the latest a retry may run is now + 100.
        let reg = f.registration(now - 4100, now - 500);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::RetryableInternal(Duration::from_secs(10_000)),
        );
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Deferred(Duration::from_secs(100)));
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 100) * 1000));
        assert!(f.live(&activity.id).await);
        f.done().await;
    }

    #[tokio::test]
    async fn skip_never_reschedules_past_the_outage_grace() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let activity = f.seed(f.registration(now - 4100, now - 500), now).await;
        let failed = StatsError::Status(503);

        let outcome = process_one(
            &f.state,
            &activity,
            Err(&failed),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Skipped);
        assert!(f.mock.get_live_activity_messages().is_empty());
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 100) * 1000));
        f.done().await;
    }

    #[tokio::test]
    async fn unauthorized_send_retries_next_cycle() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 900, now + 2700);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::Unauthorized("no apns key".into()),
        );
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        let cadence = Duration::from_secs(f.cadence());
        assert_eq!(outcome, ProcessOutcome::Deferred(cadence));
        assert_eq!(
            f.due_ms(&activity.id).await,
            Some((now + f.cadence()) * 1000)
        );
        assert!(f.live(&activity.id).await);
        assert_eq!(f.hget(&activity.id, "last_content").await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn retry_next_cycle_never_passes_the_outage_grace() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 4100, now - 500);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::Unauthorized("no apns key".into()),
        );
        let activity = f.seed(reg, now).await;

        // Past the window, so this is the summary end; its send fails.
        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(5), 2, 1)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Deferred(Duration::from_secs(100)));
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 100) * 1000));
        assert!(f.live(&activity.id).await);
        f.done().await;
    }

    #[tokio::test]
    async fn failed_end_send_defers_and_stays_live() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 3600, now);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::RetryableInternal(Duration::from_secs(30)),
        );
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(40), 6, 2)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Deferred(Duration::from_secs(30)));
        assert!(f.live(&activity.id).await);
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 30) * 1000));
        assert!(f.mock.get_live_activity_messages().is_empty());
        f.done().await;
    }

    #[tokio::test]
    async fn token_not_registered_on_end_finishes() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let reg = f.registration(now - 3600, now);
        f.mock
            .set_error_for_token(&reg.activity_token, FcmError::TokenNotRegistered);
        let activity = f.seed(reg, now).await;

        let outcome = process_one(
            &f.state,
            &activity,
            Ok(&f.stats(Some(40), 6, 2)),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Finished);
        assert!(!f.live(&activity.id).await);
        assert_eq!(f.due_ms(&activity.id).await, None);
        f.done().await;
    }

    #[tokio::test]
    async fn outage_end_without_last_content_shows_unknown_counts() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        let activity = f.seed(f.registration(now - 4200, now - 600), now).await;
        let failed = StatsError::Request("timeout".into());

        let outcome = process_one(
            &f.state,
            &activity,
            Err(&failed),
            ViewsLookup::NotNeeded,
            now,
        )
        .await;

        assert_eq!(outcome, ProcessOutcome::Ended);
        let sent = f.mock.get_live_activity_messages();
        assert_eq!(sent.len(), 1);
        let m = &sent[0];
        assert_eq!(
            m.event,
            LiveActivityEvent::End {
                dismissal_unix: now - 1
            }
        );
        let expected = ContentState {
            phase: Phase::Live,
            views: None,
            likes: None,
            comments: None,
            updated_at: now,
            show_first_run_note: true,
            post_coordinate: activity.registration.post_coordinate.clone(),
            post_event_id: f.event_id.clone(),
        };
        assert_eq!(content(m), expected);
        assert!(!f.live(&activity.id).await);
        f.done().await;
    }
}
