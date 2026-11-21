//! The Live Activity worker. [`run`] loops over [`tick`], which claims due
//! activities, fetches their stats in one bulk call plus any `/views` fallback,
//! and hands each one to [`process_one`]: decide what to show, send it through
//! Firebase, then reschedule or finish the activity. Nothing here touches device
//! registration (`user_tokens:*`, `token_to_pubkey`): a dead token finishes the
//! activity only.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, StreamExt};
use futures_util::FutureExt;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::{Result, ServiceError};
use crate::fcm_sender::{
    live_activity_send_outcome, token_prefix, FcmError, LiveActivityEvent, LiveActivityMessage,
    SendOutcome,
};
use crate::live_activity::content::{
    decide, needs_views_fallback, outage_end, ContentState, Decision,
};
use crate::live_activity::stats_client::{BulkStats, StatsClient, StatsError};
use crate::live_activity::store::{self, StoredActivity};
use crate::metrics;
use crate::state::AppState;

/// Seconds after a send that iOS marks the activity stale and APNs drops an
/// undelivered update.
pub const STALE_AFTER_SECS: u64 = 600;
/// Seconds after an end that iOS removes the activity from the Lock Screen.
pub const DISMISS_AFTER_SECS: u64 = 1800;
/// Shortest wait before a failed send is retried. Firebase answers a 5xx or 429
/// without `Retry-After` with a 1 s retry; honouring that would resend every
/// active activity about once a second while Firebase is failing.
pub const LIVE_ACTIVITY_MIN_RETRY: Duration = Duration::from_secs(30);
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
    /// The activity was finished without an end send (dead token, or no longer
    /// live).
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
    // Reported like a payload Firebase rejected. ContentState is plain data, so
    // serializing it does not fail in practice.
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
        // Firebase's delay is kept between the floor and one cadence.
        SendOutcome::RetryAfter(delay) => delay.max(LIVE_ACTIVITY_MIN_RETRY).min(cadence),
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

/// Most activities claimed by one tick.
pub const CLAIM_LIMIT: usize = 100;
/// Back-off when Funnelcake rate limits without a usable `Retry-After`.
const DEFAULT_RATE_LIMIT_RETRY_SECS: u64 = 60;
/// Window the `/views` budget counts over.
const VIEWS_BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Activities claimed from the schedule.
    pub claimed: usize,
    /// Updates sent.
    pub sent: usize,
    /// Ends sent.
    pub ended: usize,
    /// Activities skipped this cycle (stats failed).
    pub skipped: usize,
    /// Activities finished without an end send.
    pub finished: usize,
    /// Activities whose send failed and is retried later.
    pub deferred: usize,
    /// Activities whose processing panicked; each is retried a cadence later.
    pub panicked: usize,
    /// Claimed activities put back unprocessed: rate limited, or past the deadline.
    pub released: usize,
    /// `/views` fallback lookups made.
    pub views_lookups: usize,
    /// Fallbacks that found the per-minute budget spent.
    pub views_budget_spent: usize,
    /// The tick stopped starting work at its deadline.
    pub deadline_hit: bool,
    /// Funnelcake rate limited this tick; the loop waits this long before the next.
    pub backoff: Option<Duration>,
    /// `ZCARD live_activity_jobs` before the claim, when it could be read.
    pub active: Option<u64>,
    /// Age of the oldest due activity before the claim (0 when none is due).
    pub oldest_due_age_secs: Option<u64>,
}

/// Per-replica budget of `/views` fallback lookups: at most `per_minute` in any
/// 60 s, so the fallback stays under Funnelcake's general rate limit.
#[derive(Debug)]
pub struct ViewsBudget {
    per_minute: usize,
    taken: std::sync::Mutex<VecDeque<SystemTime>>,
}

impl ViewsBudget {
    /// A budget of `per_minute` lookups in any rolling minute.
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute: usize::try_from(per_minute).unwrap_or(usize::MAX),
            taken: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// Takes one lookup if the last minute has room. Lookups stamped after
    /// `now` (the clock stepped back) no longer count.
    pub fn try_take(&self, now: SystemTime) -> bool {
        let mut taken = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        taken.retain(|at| {
            now.duration_since(*at)
                .is_ok_and(|age| age < VIEWS_BUDGET_WINDOW)
        });
        if taken.len() >= self.per_minute {
            return false;
        }
        taken.push_back(now);
        true
    }
}

/// Runs the worker until `shutdown` is cancelled.
///
/// A tick that fails or panics is logged and counted, and the loop goes on: the
/// worker never takes the service down and returns only on shutdown. Only a
/// tick that claimed a full batch is followed straight by the next; otherwise
/// it sleeps `poll_millis`, and after a rate-limited tick it waits out
/// Funnelcake's back-off (see `pause_after`). Shutdown cancels a tick
/// mid-flight; the activities it had claimed come back once their lease
/// expires, and the update or end that may then be sent again is harmless.
pub async fn run(state: Arc<AppState>, stats: StatsClient, shutdown: CancellationToken) {
    let settings = &state.settings.live_activity;
    let budget = ViewsBudget::new(settings.views_lookups_per_minute);
    let poll = Duration::from_millis(settings.poll_millis);
    info!("Starting Live Activity worker");
    loop {
        let ticked = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            ticked = AssertUnwindSafe(tick(&state, &stats, &budget, SystemTime::now()))
                .catch_unwind() => ticked,
        };
        let pause = match ticked {
            Ok(Ok(report)) => {
                if report.claimed > 0 {
                    debug!(?report, "live activity tick finished");
                }
                pause_after(&report, poll)
            }
            Ok(Err(e)) => {
                error!(error = %e, "live activity tick failed; retrying after the poll interval");
                metrics::live_activity_skipped("tick_failed");
                Some(poll)
            }
            Err(_) => {
                error!("live activity tick panicked; retrying after the poll interval");
                metrics::live_activity_skipped("tick_panicked");
                Some(poll)
            }
        };
        // Set on every finished pass, whatever its outcome, so a worker that
        // hangs or stops looping shows up as a gauge that stops advancing.
        let finished_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        metrics::live_activity_last_tick(finished_at as f64);
        if let Some(pause) = pause {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }
    info!("Live Activity worker stopped");
}

/// How long the loop waits after a successful tick.
///
/// A tick that claimed a full batch ([`CLAIM_LIMIT`]) may have left more due
/// work, so the next tick starts at once. Any other tick is followed by at least
/// `poll`, so the bulk request rate is bounded by the poll interval rather than
/// by how fast ticks finish. A rate-limited tick waits out the back-off, and
/// never less than `poll`.
fn pause_after(report: &TickReport, poll: Duration) -> Option<Duration> {
    if let Some(backoff) = report.backoff {
        return Some(backoff.max(poll));
    }
    (report.claimed < CLAIM_LIMIT).then_some(poll)
}

/// One pass: sample the gauges, claim due activities, look up their stats in one
/// bulk call (plus any `/views` fallbacks), then process each under
/// `catch_unwind`, at most `max_concurrency` at a time.
///
/// The tick stops starting work at `tick_deadline_secs`; activities not started
/// by then are put back as due at `now`. Work already in flight finishes, which
/// config validation keeps inside the claim's lease. A rate-limited bulk lookup
/// pushes the claimed set back by Funnelcake's `Retry-After` (60 s by default,
/// at most a cadence), except activities already past their outage grace, which
/// end now. Returns an error only when the clock reads before the Unix epoch
/// or the claim fails.
pub async fn tick(
    state: &AppState,
    stats: &StatsClient,
    budget: &ViewsBudget,
    now: SystemTime,
) -> Result<TickReport> {
    let settings = &state.settings.live_activity;
    let deadline = Instant::now() + Duration::from_secs(settings.tick_deadline_secs);
    let since_epoch = now
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ServiceError::Internal(format!("system clock before Unix epoch: {e}")))?;
    let now_unix = since_epoch.as_secs();
    let now_ms = u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX);
    let mut report = TickReport::default();

    // Sampled before the claim, which leases the due head into the future.
    sample_gauges(state, now_ms, &mut report).await;

    let claimed = store::claim_due(
        &state.redis_pool,
        now_ms,
        settings.lease_secs.saturating_mul(1000),
        CLAIM_LIMIT,
    )
    .await?;
    report.claimed = claimed.len();
    if claimed.is_empty() {
        return Ok(report);
    }

    let groups: Vec<Vec<String>> = claimed
        .iter()
        .map(|a| a.registration.post_event_ids.clone())
        .collect();
    let Ok(results) = tokio::time::timeout_at(deadline, stats.bulk_grouped(&groups)).await else {
        warn!(
            claimed = claimed.len(),
            "live activity stats lookup outlasted the tick deadline"
        );
        report.deadline_hit = true;
        release(state, &claimed, now_ms, &mut report).await;
        return Ok(report);
    };
    count_stats_failures(&results);

    let mut work: Vec<(StoredActivity, std::result::Result<BulkStats, StatsError>)> =
        Vec::with_capacity(claimed.len());
    match rate_limit_backoff(&results, cadence_secs(state)) {
        Some(backoff) => {
            report.backoff = Some(backoff);
            let backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX);
            for (activity, result) in claimed.into_iter().zip(results) {
                if past_outage_grace(&activity, now_unix) {
                    // Waiting longer could outlive the hash; end it now.
                    work.push((activity, result));
                    continue;
                }
                let due_ms = cap_due_ms(&activity, now_unix, now_ms.saturating_add(backoff_ms));
                schedule(state, &activity, due_ms).await;
                report.released += 1;
            }
            warn!(
                pushed_back = report.released,
                backoff_secs = backoff.as_secs(),
                "Funnelcake rate limited live activity stats; pushing the due set back"
            );
        }
        None => work.extend(claimed.into_iter().zip(results)),
    }

    let Some(views) = views_lookups(state, stats, budget, &work, now, deadline, &mut report).await
    else {
        warn!("live activity views lookups outlasted the tick deadline");
        report.deadline_hit = true;
        let unstarted: Vec<StoredActivity> = work.into_iter().map(|(a, _)| a).collect();
        release(state, &unstarted, now_ms, &mut report).await;
        return Ok(report);
    };

    let concurrency = settings.max_concurrency.max(1);
    let outcomes: Vec<Option<std::result::Result<ProcessOutcome, ()>>> =
        stream::iter(work.into_iter().zip(views))
            .map(|((activity, result), views)| async move {
                if Instant::now() >= deadline {
                    schedule(state, &activity, now_ms).await;
                    return None;
                }
                let processed = AssertUnwindSafe(process_one(
                    state,
                    &activity,
                    result.as_ref(),
                    views,
                    now_unix,
                ))
                .catch_unwind()
                .await;
                Some(match processed {
                    Ok(outcome) => Ok(outcome),
                    Err(_) => {
                        error!(
                            activity_id = %activity.id,
                            owner = %activity.owner,
                            "live activity processing panicked; retrying next cycle"
                        );
                        metrics::live_activity_skipped("panicked");
                        let next = now_ms.saturating_add(cadence_secs(state).saturating_mul(1000));
                        schedule(state, &activity, cap_due_ms(&activity, now_unix, next)).await;
                        Err(())
                    }
                })
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

    for outcome in outcomes {
        match outcome {
            None => {
                report.deadline_hit = true;
                report.released += 1;
            }
            Some(Err(())) => report.panicked += 1,
            Some(Ok(ProcessOutcome::Sent)) => report.sent += 1,
            Some(Ok(ProcessOutcome::Ended)) => report.ended += 1,
            Some(Ok(ProcessOutcome::Skipped)) => report.skipped += 1,
            Some(Ok(ProcessOutcome::Finished)) => report.finished += 1,
            Some(Ok(ProcessOutcome::Deferred(_))) => report.deferred += 1,
        }
    }
    Ok(report)
}

async fn sample_gauges(state: &AppState, now_ms: u64, report: &mut TickReport) {
    match store::active_count(&state.redis_pool).await {
        Ok(active) => {
            metrics::live_activity_active(active as f64);
            report.active = Some(active);
        }
        Err(e) => warn!(error = %e, "could not read the live activity count"),
    }
    match store::oldest_due_age_secs(&state.redis_pool, now_ms).await {
        Ok(age) => {
            let age = age.unwrap_or(0);
            metrics::live_activity_oldest_due_age(age as f64);
            report.oldest_due_age_secs = Some(age);
        }
        Err(e) => warn!(error = %e, "could not read the oldest due live activity"),
    }
}

fn past_outage_grace(activity: &StoredActivity, now_unix: u64) -> bool {
    now_unix
        >= activity
            .registration
            .window_ends_at
            .saturating_add(OUTAGE_GRACE_SECS)
}

/// Puts unstarted activities back as due at `now_ms`, so the next tick takes
/// them instead of waiting out the lease.
async fn release(
    state: &AppState,
    activities: &[StoredActivity],
    now_ms: u64,
    report: &mut TickReport,
) {
    for activity in activities {
        schedule(state, activity, now_ms).await;
        report.released += 1;
    }
}

fn stats_failure_reason(error: &StatsError) -> &'static str {
    match error {
        StatsError::Request(_) => "request",
        StatsError::Status(_) => "status",
        StatsError::RateLimited { .. } => "rate_limited",
        StatsError::Decode(_) => "decode",
    }
}

/// Counts each distinct failure once. A failed bulk request fails every group
/// packed into it with the same error, so this approximates failed requests.
fn count_stats_failures(results: &[std::result::Result<BulkStats, StatsError>]) {
    let mut seen: Vec<&StatsError> = Vec::new();
    for error in results.iter().filter_map(|r| r.as_ref().err()) {
        if !seen.contains(&error) {
            seen.push(error);
            metrics::live_activity_stats_failure(stats_failure_reason(error));
        }
    }
}

/// The back-off when any group was rate limited: the longest `Retry-After`
/// (60 s when absent), kept between 1 s and one cadence.
fn rate_limit_backoff(
    results: &[std::result::Result<BulkStats, StatsError>],
    cadence_secs: u64,
) -> Option<Duration> {
    results
        .iter()
        .filter_map(|r| match r {
            Err(StatsError::RateLimited { retry_after }) => {
                Some(retry_after.unwrap_or(Duration::from_secs(DEFAULT_RATE_LIMIT_RETRY_SECS)))
            }
            _ => None,
        })
        .max()
        .map(|d| {
            d.clamp(
                Duration::from_secs(1),
                Duration::from_secs(cadence_secs.max(1)),
            )
        })
}

/// The `/views` fallback for each work item, in order. Lookups run concurrently,
/// at most `max_concurrency` at a time, against the per-minute budget. Returns
/// `None` when they outlast the tick deadline.
async fn views_lookups(
    state: &AppState,
    stats: &StatsClient,
    budget: &ViewsBudget,
    work: &[(StoredActivity, std::result::Result<BulkStats, StatsError>)],
    now: SystemTime,
    deadline: Instant,
    report: &mut TickReport,
) -> Option<Vec<ViewsLookup>> {
    let mut lookups = vec![ViewsLookup::NotNeeded; work.len()];
    let mut wanted: Vec<(usize, String)> = Vec::new();
    for (i, (activity, result)) in work.iter().enumerate() {
        let Ok(bulk) = result else { continue };
        if !needs_views_fallback(activity, bulk) {
            continue;
        }
        let Some(first_id) = activity.registration.post_event_ids.first() else {
            continue;
        };
        if budget.try_take(now) {
            wanted.push((i, first_id.clone()));
        } else {
            metrics::live_activity_views_lookup("budget_spent");
            report.views_budget_spent += 1;
            lookups[i] = ViewsLookup::BudgetSpent;
        }
    }
    if wanted.is_empty() {
        return Some(lookups);
    }
    report.views_lookups = wanted.len();
    let fetched = tokio::time::timeout_at(
        deadline,
        stream::iter(wanted)
            .map(|(i, id)| async move { (i, stats.views(&id).await) })
            .buffer_unordered(state.settings.live_activity.max_concurrency.max(1))
            .collect::<Vec<_>>(),
    )
    .await
    .ok()?;
    for (i, result) in fetched {
        lookups[i] = match result {
            Ok(views) => {
                metrics::live_activity_views_lookup("ok");
                ViewsLookup::Found(views)
            }
            Err(e) => {
                metrics::live_activity_views_lookup("failed");
                metrics::live_activity_stats_failure(stats_failure_reason(&e));
                ViewsLookup::Failed
            }
        };
    }
    Some(lookups)
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

    #[test]
    fn pauses_after_a_partial_claim_but_not_a_full_batch() {
        let poll = Duration::from_millis(500);
        let report = |claimed| TickReport {
            claimed,
            ..TickReport::default()
        };
        assert_eq!(pause_after(&report(0), poll), Some(poll));
        assert_eq!(pause_after(&report(1), poll), Some(poll));
        assert_eq!(pause_after(&report(CLAIM_LIMIT - 1), poll), Some(poll));
        assert_eq!(pause_after(&report(CLAIM_LIMIT), poll), None);
    }

    #[test]
    fn rate_limit_backoff_pauses_at_least_the_poll_interval() {
        let poll = Duration::from_millis(500);
        let backed_off = |claimed, backoff| TickReport {
            claimed,
            backoff: Some(backoff),
            ..TickReport::default()
        };
        let long = Duration::from_secs(60);
        assert_eq!(
            pause_after(&backed_off(CLAIM_LIMIT, long), poll),
            Some(long)
        );
        assert_eq!(
            pause_after(&backed_off(3, Duration::from_millis(10)), poll),
            Some(poll)
        );
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
    async fn short_retry_after_is_floored() {
        for delay in [Duration::from_secs(1), Duration::ZERO] {
            let Some(f) = Fixture::new().await else {
                return;
            };
            let now = now_secs();
            let reg = f.registration(now - 900, now + 2700);
            f.mock
                .set_error_for_token(&reg.activity_token, FcmError::RetryableInternal(delay));
            let activity = f.seed(reg, now).await;

            let outcome = process_one(
                &f.state,
                &activity,
                Ok(&f.stats(Some(5), 2, 1)),
                ViewsLookup::NotNeeded,
                now,
            )
            .await;

            assert_eq!(
                outcome,
                ProcessOutcome::Deferred(LIVE_ACTIVITY_MIN_RETRY),
                "retry-after {delay:?}"
            );
            assert_eq!(f.due_ms(&activity.id).await, Some((now + 30) * 1000));
            assert!(f.live(&activity.id).await);
            f.done().await;
        }
    }

    #[tokio::test]
    async fn outage_grace_cap_wins_over_the_retry_floor() {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let now = now_secs();
        // The window closed 590 s ago: the latest a retry may run is now + 10.
        let reg = f.registration(now - 4190, now - 590);
        f.mock.set_error_for_token(
            &reg.activity_token,
            FcmError::RetryableInternal(Duration::from_secs(1)),
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

        assert_eq!(outcome, ProcessOutcome::Deferred(Duration::from_secs(10)));
        assert_eq!(f.due_ms(&activity.id).await, Some((now + 10) * 1000));
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
    /// Tests of `tick` and `run`. A tick claims every due activity in Redis, so
    /// these run in their own logical database, flushed at the start of each
    /// test and serialized by [`TICK_DB`], so no other test's leftovers are claimed.
    mod looped {
        use super::*;
        use crate::fcm_sender::{FcmSend, LiveActivityMessage};
        use crate::models::FcmPayload;
        use async_trait::async_trait;
        use metrics_exporter_prometheus::{
            PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
        };
        use std::collections::HashMap;
        use std::sync::{LazyLock, Mutex};

        static TICK_DB: LazyLock<tokio::sync::Mutex<()>> =
            LazyLock::new(|| tokio::sync::Mutex::new(()));

        /// Logical database used only by these tests.
        const TICK_DB_INDEX: u8 = 9;

        /// Funnelcake stand-in: ids in `present` are returned with their counts,
        /// every other id asked for is reported missing.
        #[derive(Default)]
        struct StubConf {
            bulk_calls: Vec<Vec<String>>,
            views_calls: Vec<String>,
            status: Option<u16>,
            retry_after: Option<&'static str>,
            delay: Duration,
            present: HashMap<String, (Option<u64>, u64, u64)>,
            views: HashMap<String, u64>,
        }

        #[derive(Clone, Default)]
        struct Stub(Arc<Mutex<StubConf>>);

        impl Stub {
            fn conf(&self) -> std::sync::MutexGuard<'_, StubConf> {
                self.0.lock().unwrap()
            }
        }

        async fn bulk(
            axum::extract::State(stub): axum::extract::State<Stub>,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let delay = stub.conf().delay;
            tokio::time::sleep(delay).await;
            let ids: Vec<String> = body["event_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let conf = {
                let mut c = stub.conf();
                c.bulk_calls.push(ids.clone());
                (c.status, c.retry_after, c.present.clone())
            };
            let (status, retry_after, present) = conf;
            if let Some(status) = status {
                let mut r = axum::http::StatusCode::from_u16(status)
                    .unwrap()
                    .into_response();
                if let Some(ra) = retry_after {
                    r.headers_mut().insert("retry-after", ra.parse().unwrap());
                }
                return r;
            }
            let mut stats = serde_json::Map::new();
            let mut missing = Vec::new();
            for id in ids {
                match present.get(&id) {
                    Some((views, reactions, comments)) => {
                        stats.insert(
                            id,
                            json!({"views": views, "reactions": reactions, "comments": comments}),
                        );
                    }
                    None => missing.push(id),
                }
            }
            axum::Json(json!({"stats": stats, "missing": missing})).into_response()
        }

        async fn views(
            axum::extract::State(stub): axum::extract::State<Stub>,
            axum::extract::Path(id): axum::extract::Path<String>,
        ) -> axum::Json<serde_json::Value> {
            let mut c = stub.conf();
            c.views_calls.push(id.clone());
            let views = c.views.get(&id).copied().unwrap_or(0);
            axum::Json(json!({"views": views}))
        }

        async fn serve(stub: Stub) -> String {
            let app = axum::Router::new()
                .route("/api/videos/stats/bulk", axum::routing::post(bulk))
                .route("/api/videos/:id/views", axum::routing::get(views))
                .with_state(stub);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            format!("http://{addr}")
        }

        /// Firebase stand-in that panics for one activity token.
        struct PanicFor {
            inner: MockFcmSender,
            activity_token: String,
        }

        #[async_trait]
        impl FcmSend for PanicFor {
            async fn send_single(
                &self,
                token: &str,
                payload: FcmPayload,
            ) -> std::result::Result<(), FcmError> {
                self.inner.send_single(token, payload).await
            }

            async fn send_live_activity(
                &self,
                message: &LiveActivityMessage,
            ) -> std::result::Result<(), FcmError> {
                if message.activity_token == self.activity_token {
                    panic!("injected Firebase panic");
                }
                self.inner.send_live_activity(message).await
            }
        }

        async fn tick_pool() -> Option<RedisPool> {
            let base =
                std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
            let mut url = url::Url::parse(&base).ok()?;
            url.set_path(&format!("/{TICK_DB_INDEX}"));
            let pool = redis_store::create_pool(url.as_str(), 10).await.ok()?;
            let mut conn = pool.get().await.ok()?;
            let flushed: redis::RedisResult<String> =
                redis::cmd("FLUSHDB").query_async(&mut *conn).await;
            drop(conn);
            flushed.ok().map(|_| pool)
        }

        fn recorder() -> (PrometheusRecorder, PrometheusHandle) {
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            (recorder, handle)
        }

        fn rendered(handle: &PrometheusHandle) -> String {
            handle.run_upkeep();
            handle.render()
        }

        /// One tick test's world: its own Redis database, stub Funnelcake and fake Firebase.
        struct World {
            _db: tokio::sync::MutexGuard<'static, ()>,
            state: AppState,
            mock: MockFcmSender,
            stub: Stub,
            stats: StatsClient,
        }

        struct Opts {
            tick_deadline_secs: u64,
            request_timeout_secs: u64,
            views_lookups_per_minute: u32,
            poll_millis: u64,
            panic_for: Option<String>,
        }

        impl Default for Opts {
            fn default() -> Self {
                Self {
                    tick_deadline_secs: 30,
                    request_timeout_secs: 5,
                    views_lookups_per_minute: 100,
                    poll_millis: 20,
                    panic_for: None,
                }
            }
        }

        impl World {
            async fn new(opts: Opts) -> Option<Self> {
                let db = TICK_DB.lock().await;
                let pool = tick_pool().await?;
                let stub = Stub::default();
                let base_url = serve(stub.clone()).await;
                let mock = MockFcmSender::new();
                let fcm: Box<dyn FcmSend> = match opts.panic_for {
                    Some(activity_token) => Box::new(PanicFor {
                        inner: mock.clone(),
                        activity_token,
                    }),
                    None => Box::new(mock.clone()),
                };
                let mut settings = crate::config::Settings::new().expect("runtime settings load");
                let la = &mut settings.live_activity;
                la.enabled = true;
                la.funnelcake_base_url = base_url;
                la.tick_deadline_secs = opts.tick_deadline_secs;
                la.request_timeout_secs = opts.request_timeout_secs;
                la.lease_secs = 120;
                la.views_lookups_per_minute = opts.views_lookups_per_minute;
                la.poll_millis = opts.poll_millis;
                let stats = StatsClient::new(&settings.live_activity).unwrap();
                let state = AppState {
                    settings,
                    redis_pool: pool,
                    fcm_client: Arc::new(FcmClient::new_with_impl(fcm)),
                    service_keys: None,
                    crypto_service: None,
                    nostr_client: Arc::new(nostr_sdk::Client::default()),
                    profile_client: Arc::new(nostr_sdk::Client::default()),
                    mention_parser_service: None,
                };
                Some(Self {
                    _db: db,
                    state,
                    mock,
                    stub,
                    stats,
                })
            }

            fn pool(&self) -> &RedisPool {
                &self.state.redis_pool
            }

            fn cadence(&self) -> u64 {
                self.state.settings.live_activity.cadence_secs
            }

            fn budget(&self) -> ViewsBudget {
                ViewsBudget::new(self.state.settings.live_activity.views_lookups_per_minute)
            }

            /// Stores an activity for a fresh owner and device, due at `due_ms`.
            async fn seed(
                &self,
                posted_at: u64,
                window_ends_at: u64,
                due_ms: u64,
                activity_token: Option<String>,
            ) -> StoredActivity {
                let owner = Keys::generate().public_key().to_hex();
                let fcm_token = format!("fcm-{}", hex(40));
                let reg = LiveActivityRegistration {
                    activity_token: activity_token.unwrap_or_else(|| hex(64)),
                    post_coordinate: format!("34236:{owner}:{}", hex(8)),
                    post_event_ids: vec![hex(64)],
                    posted_at,
                    window_ends_at,
                    show_first_run_note: false,
                    state: ActivityState::Active,
                };
                let outcome = store::upsert_active(
                    self.pool(),
                    &owner,
                    &fcm_token,
                    &reg,
                    posted_at,
                    &hex(64),
                    due_ms,
                    5,
                    100_000,
                )
                .await
                .unwrap();
                assert_eq!(outcome, store::UpsertOutcome::Created);
                StoredActivity {
                    id: store::activity_id(&owner, &reg.activity_token),
                    owner,
                    fcm_token,
                    first_seen_at: posted_at,
                    absent_checks: 0,
                    last_content: None,
                    registration: reg,
                }
            }

            fn present(&self, a: &StoredActivity, views: Option<u64>, likes: u64, comments: u64) {
                self.stub.conf().present.insert(
                    a.registration.post_event_ids[0].clone(),
                    (views, likes, comments),
                );
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

            async fn tick(&self, budget: &ViewsBudget, now_unix: u64) -> TickReport {
                tick(
                    &self.state,
                    &self.stats,
                    budget,
                    UNIX_EPOCH + Duration::from_secs(now_unix),
                )
                .await
                .unwrap()
            }

            fn sent_to(&self, a: &StoredActivity) -> Vec<LiveActivityMessage> {
                self.mock
                    .get_live_activity_messages()
                    .into_iter()
                    .filter(|m| m.activity_token == a.registration.activity_token)
                    .collect()
            }
        }

        fn in_skip_window(due_ms: Option<u64>, now: u64, cadence: u64) -> bool {
            let base = (now + cadence) * 1000;
            due_ms.is_some_and(|d| (base - 15_000..=base + 15_000).contains(&d))
        }

        #[test]
        fn views_budget_allows_n_per_rolling_minute() {
            let budget = ViewsBudget::new(2);
            let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
            assert!(budget.try_take(t));
            assert!(budget.try_take(t + Duration::from_secs(10)));
            assert!(!budget.try_take(t + Duration::from_secs(59)));
            // The first lookup is a minute old: one slot is free again.
            assert!(budget.try_take(t + Duration::from_secs(60)));
            assert!(!budget.try_take(t + Duration::from_secs(61)));
            // A clock stepped backwards does not freeze the budget.
            assert!(budget.try_take(t - Duration::from_secs(3600)));
        }

        #[tokio::test]
        async fn tick_batches_one_bulk_call_for_many_activities() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let now = now_secs();
            let mut seeded = Vec::new();
            for i in 0..6 {
                let a = w.seed(now - 900, now + 2700, now * 1000, None).await;
                w.present(&a, Some(10 + i), i, 1);
                seeded.push(a);
            }

            let report = w.tick(&w.budget(), now).await;

            assert_eq!(report.claimed, 6);
            assert_eq!(report.sent, 6);
            let calls = w.stub.conf().bulk_calls.clone();
            assert_eq!(calls.len(), 1, "one bulk request for the whole batch");
            let mut asked = calls[0].clone();
            asked.sort();
            let mut expected: Vec<String> = seeded
                .iter()
                .map(|a| a.registration.post_event_ids[0].clone())
                .collect();
            expected.sort();
            assert_eq!(asked, expected);
            for (i, a) in seeded.iter().enumerate() {
                let sent = w.sent_to(a);
                assert_eq!(sent.len(), 1);
                assert_eq!(content(&sent[0]).views, Some(10 + i as u64));
                assert_eq!(w.due_ms(&a.id).await, Some((now + w.cadence()) * 1000));
            }
        }

        #[tokio::test]
        async fn stats_failure_skips_send_and_reschedules() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let (recorder, handle) = recorder();
            let _metrics = ::metrics::set_default_local_recorder(&recorder);
            w.stub.conf().status = Some(500);
            let now = now_secs();
            let a = w.seed(now - 900, now + 2700, now * 1000, None).await;
            let b = w.seed(now - 900, now + 2700, now * 1000, None).await;

            let report = w.tick(&w.budget(), now).await;

            assert_eq!(report.skipped, 2);
            assert!(w.mock.get_live_activity_messages().is_empty());
            for x in [&a, &b] {
                assert!(w.live(&x.id).await);
                assert!(in_skip_window(w.due_ms(&x.id).await, now, w.cadence()));
            }
            let text = rendered(&handle);
            assert!(
                text.contains(r#"push_live_activity_stats_failures_total{reason="status"} 1"#),
                "{text}"
            );
        }

        #[tokio::test]
        async fn panic_in_one_activity_is_contained() {
            let doomed_token = hex(64);
            let Some(w) = World::new(Opts {
                panic_for: Some(doomed_token.clone()),
                ..Opts::default()
            })
            .await
            else {
                return;
            };
            let now = now_secs();
            let doomed = w
                .seed(now - 900, now + 2700, now * 1000, Some(doomed_token))
                .await;
            let a = w.seed(now - 900, now + 2700, now * 1000, None).await;
            let b = w.seed(now - 900, now + 2700, now * 1000, None).await;
            for x in [&doomed, &a, &b] {
                w.present(x, Some(5), 1, 0);
            }
            let budget = w.budget();

            let report = w.tick(&budget, now).await;

            assert_eq!(report.panicked, 1);
            assert_eq!(report.sent, 2);
            assert_eq!(w.sent_to(&a).len(), 1);
            assert_eq!(w.sent_to(&b).len(), 1);
            assert!(w.live(&doomed.id).await);
            assert_eq!(w.due_ms(&doomed.id).await, Some((now + w.cadence()) * 1000));

            // The next cycle still runs every activity.
            let later = now + w.cadence();
            let report = w.tick(&budget, later).await;
            assert_eq!((report.claimed, report.sent, report.panicked), (3, 2, 1));
            assert_eq!(w.sent_to(&a).len(), 2);
        }

        /// The loop records each finished tick, so a stalled worker shows up as
        /// a gauge that stops advancing even when nothing is due.
        #[tokio::test]
        async fn run_records_the_last_tick_time() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let (recorder, handle) = recorder();
            let _metrics = ::metrics::set_default_local_recorder(&recorder);
            let World { _db, state, .. } = w;
            let state = Arc::new(state);
            let stats = StatsClient::new(&state.settings.live_activity).unwrap();
            let before = now_secs();
            let shutdown = CancellationToken::new();
            let worker = tokio::spawn(run(Arc::clone(&state), stats, shutdown.clone()));

            let started = tokio::time::Instant::now();
            let value = loop {
                let text = rendered(&handle);
                if let Some(v) = text
                    .lines()
                    .find_map(|l| l.strip_prefix("push_live_activity_last_tick_unix "))
                {
                    break v.trim().parse::<f64>().unwrap();
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "no tick recorded"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert!(
                value >= before as f64 && value <= now_secs() as f64,
                "{value}"
            );

            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), worker)
                .await
                .expect("worker stops on shutdown")
                .unwrap();
        }

        #[tokio::test]
        async fn tick_error_keeps_loop_running() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let (recorder, handle) = recorder();
            let _metrics = ::metrics::set_default_local_recorder(&recorder);
            // A schedule key of the wrong type makes every claim fail.
            {
                let mut conn = w.pool().get().await.unwrap();
                let _: () = redis::cmd("SET")
                    .arg("live_activity_jobs")
                    .arg("not a sorted set")
                    .query_async(&mut *conn)
                    .await
                    .unwrap();
            }
            let World {
                _db,
                state,
                mock,
                stub,
                stats,
            } = w;
            let state = Arc::new(state);
            let shutdown = CancellationToken::new();
            let worker = tokio::spawn(run(Arc::clone(&state), stats, shutdown.clone()));

            let failed = r#"push_live_activity_skipped_total{reason="tick_failed"}"#;
            let started = tokio::time::Instant::now();
            while !rendered(&handle).contains(failed) {
                assert!(started.elapsed() < Duration::from_secs(5), "no tick failed");
                assert!(!worker.is_finished(), "worker exited on a tick error");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // Repair Redis: the same loop picks up new work.
            {
                let mut conn = state.redis_pool.get().await.unwrap();
                let _: () = redis::cmd("DEL")
                    .arg("live_activity_jobs")
                    .query_async(&mut *conn)
                    .await
                    .unwrap();
            }
            let now = now_secs();
            let owner = Keys::generate().public_key().to_hex();
            let reg = LiveActivityRegistration {
                activity_token: hex(64),
                post_coordinate: format!("34236:{owner}:{}", hex(8)),
                post_event_ids: vec![hex(64)],
                posted_at: now - 900,
                window_ends_at: now + 2700,
                show_first_run_note: false,
                state: ActivityState::Active,
            };
            stub.conf()
                .present
                .insert(reg.post_event_ids[0].clone(), (Some(3), 0, 0));
            store::upsert_active(
                &state.redis_pool,
                &owner,
                &format!("fcm-{}", hex(40)),
                &reg,
                now,
                &hex(64),
                now * 1000,
                5,
                100_000,
            )
            .await
            .unwrap();
            while mock.get_live_activity_messages().is_empty() {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "loop stopped working"
                );
                assert!(!worker.is_finished());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(5), worker)
                .await
                .expect("worker stops on shutdown")
                .unwrap();
        }

        #[tokio::test]
        async fn slow_stats_respects_tick_deadline() {
            let Some(w) = World::new(Opts {
                tick_deadline_secs: 1,
                request_timeout_secs: 10,
                ..Opts::default()
            })
            .await
            else {
                return;
            };
            w.stub.conf().delay = Duration::from_secs(4);
            let now = now_secs();
            let a = w.seed(now - 900, now + 2700, now * 1000, None).await;
            let b = w.seed(now - 900, now + 2700, now * 1000, None).await;

            let started = std::time::Instant::now();
            let report = w.tick(&w.budget(), now).await;
            let took = started.elapsed();

            assert!(report.deadline_hit);
            assert!(
                took < Duration::from_millis(2500),
                "tick took {took:?}, past its 1 s deadline"
            );
            assert_eq!(report.released, 2);
            assert!(w.mock.get_live_activity_messages().is_empty());
            // Put back as due at the tick's time, not left on the lease.
            for x in [&a, &b] {
                assert_eq!(w.due_ms(&x.id).await, Some(now * 1000));
                assert!(w.live(&x.id).await);
            }
        }

        #[tokio::test]
        async fn rate_limited_pushes_due_set_back() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let (recorder, handle) = recorder();
            let _metrics = ::metrics::set_default_local_recorder(&recorder);
            {
                let mut c = w.stub.conf();
                c.status = Some(429);
                c.retry_after = Some("7");
            }
            let now = now_secs();
            let a = w.seed(now - 900, now + 2700, now * 1000, None).await;
            let b = w.seed(now - 900, now + 2700, now * 1000, None).await;
            // Past the outage grace: it ends now rather than waiting on Funnelcake.
            let over = w.seed(now - 4300, now - 700, now * 1000, None).await;

            let report = w.tick(&w.budget(), now).await;

            assert_eq!(report.backoff, Some(Duration::from_secs(7)));
            assert_eq!(report.released, 2);
            assert_eq!(w.stub.conf().bulk_calls.len(), 1);
            for x in [&a, &b] {
                assert!(w.sent_to(x).is_empty());
                assert_eq!(w.due_ms(&x.id).await, Some((now + 7) * 1000));
                assert!(w.live(&x.id).await);
            }
            let ended = w.sent_to(&over);
            assert_eq!(ended.len(), 1);
            assert_eq!(
                ended[0].event,
                LiveActivityEvent::End {
                    dismissal_unix: now - 1
                }
            );
            assert!(!w.live(&over.id).await);
            let text = rendered(&handle);
            assert!(
                text.contains(
                    r#"push_live_activity_stats_failures_total{reason="rate_limited"} 1"#
                ),
                "{text}"
            );
        }

        #[tokio::test]
        async fn end_twice_is_noop() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let now = now_secs();
            let a = w.seed(now - 3600, now, now * 1000, None).await;
            w.present(&a, Some(40), 6, 2);
            let budget = w.budget();

            let report = w.tick(&budget, now).await;
            assert_eq!(report.ended, 1);
            assert_eq!(w.sent_to(&a).len(), 1);

            // A second worker whose lease expired mid-send still holds the job.
            {
                let mut conn = w.pool().get().await.unwrap();
                let _: () = redis::cmd("ZADD")
                    .arg("live_activity_jobs")
                    .arg(now * 1000)
                    .arg(&a.id)
                    .query_async(&mut *conn)
                    .await
                    .unwrap();
            }
            let report = w.tick(&budget, now).await;
            assert_eq!(report.claimed, 0);
            let outcome = process_one(
                &w.state,
                &a,
                Ok(&BulkStats::default()),
                ViewsLookup::NotNeeded,
                now,
            )
            .await;
            assert_eq!(outcome, ProcessOutcome::Finished);
            assert_eq!(w.sent_to(&a).len(), 1, "no second end");
            assert_eq!(w.due_ms(&a.id).await, None);
        }

        #[tokio::test]
        async fn views_lookups_respect_per_minute_budget() {
            let Some(w) = World::new(Opts {
                views_lookups_per_minute: 2,
                ..Opts::default()
            })
            .await
            else {
                return;
            };
            let (recorder, handle) = recorder();
            let _metrics = ::metrics::set_default_local_recorder(&recorder);
            let now = now_secs();
            let mut seeded = Vec::new();
            for i in 0..3 {
                let a = w.seed(now - 900, now + 2700, now * 1000, None).await;
                // Bulk reports null views, so each needs the /views fallback.
                w.present(&a, None, 1, 0);
                w.stub
                    .conf()
                    .views
                    .insert(a.registration.post_event_ids[0].clone(), 11 * (i + 1));
                seeded.push(a);
            }
            let budget = w.budget();

            let report = w.tick(&budget, now).await;

            assert_eq!(report.sent, 3);
            assert_eq!((report.views_lookups, report.views_budget_spent), (2, 1));
            assert_eq!(w.stub.conf().views_calls.len(), 2);
            let mut shown: Vec<Option<u64>> = seeded
                .iter()
                .map(|a| content(&w.sent_to(a)[0]).views)
                .collect();
            shown.sort();
            // The lookup that found no budget leaves views unknown: nothing was sent before.
            assert_eq!(shown.iter().filter(|v| v.is_none()).count(), 1);
            assert_eq!(shown.iter().filter(|v| v.is_some()).count(), 2);
            let text = rendered(&handle);
            for expected in [
                r#"push_live_activity_views_lookups_total{result="ok"} 2"#,
                r#"push_live_activity_views_lookups_total{result="budget_spent"} 1"#,
            ] {
                assert!(text.contains(expected), "{expected}\n{text}");
            }

            // A cadence later the minute has passed and the budget is back.
            let report = w.tick(&budget, now + w.cadence()).await;
            assert_eq!((report.views_lookups, report.views_budget_spent), (2, 1));
            assert_eq!(w.stub.conf().views_calls.len(), 4);
        }

        #[tokio::test]
        async fn gauges_report_active_and_oldest_due_age() {
            let Some(w) = World::new(Opts::default()).await else {
                return;
            };
            let (recorder, handle) = recorder();
            let _metrics = ::metrics::set_default_local_recorder(&recorder);
            let now = now_secs();
            let a = w.seed(now - 900, now + 2700, (now - 30) * 1000, None).await;
            let b = w.seed(now - 900, now + 2700, (now - 10) * 1000, None).await;
            w.seed(now - 900, now + 2700, FAR_DUE_MS, None).await;
            w.present(&a, Some(1), 0, 0);
            w.present(&b, Some(1), 0, 0);

            let report = w.tick(&w.budget(), now).await;

            assert_eq!(report.active, Some(3));
            assert_eq!(report.oldest_due_age_secs, Some(30));
            let text = rendered(&handle);
            for expected in [
                "push_live_activity_active 3",
                "push_live_activity_oldest_due_age_seconds 30",
            ] {
                assert!(text.contains(expected), "{expected}\n{text}");
            }

            // Nothing due: the age gauge drops to zero.
            let report = w.tick(&w.budget(), now + 1).await;
            assert_eq!(report.claimed, 0);
            assert_eq!(report.oldest_due_age_secs, Some(0));
            assert!(rendered(&handle).contains("push_live_activity_oldest_due_age_seconds 0"));
        }
    }
}
