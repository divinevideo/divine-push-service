# Notification delivery policy

Social pushes (direct messages included), like/repost digests, and campaigns
share one per-account interruption policy, reserved atomically in Redis across
replicas and device tokens:

- At most two pushes between visits (`absence_cap`). An authenticated
  foreground visit, reported through `POST /internal/v1/activity`, resets the
  count. The count has no expiry, so an account whose client never reports a
  visit is not reset: after two pushes it receives none until a reporting
  client opens.
- At least six hours between any two pushes (`min_interval_secs`). A visit
  does not reset this gap.
- No push while any of the account's devices is in the foreground. Presence
  lapses 180 seconds after a session's last foreground report, so a crashed
  app cannot suppress pushes indefinitely.
- The older rolling bounds of five per hour and twenty per day still apply.

Retries reuse their identity and keep their allowance and visit generation;
failed or ambiguous provider attempts conservatively keep their slot. Work
captured before a visit is dropped rather than sent after it. A comment,
mention, new-post, or direct-message push refused by this policy is dropped,
not queued. Likes and reposts buffer from the first interaction into one
`activityDigest` push per account, visit, and two-hour bucket; a digest refused
only by the six-hour gap is requeued for the remaining gap, and one refused for
any other reason is dropped.

`service.delivery` accepts `quiet_start_hour` (21), `quiet_end_hour` (7),
`hourly_cap` (5), `daily_cap` (20), `max_event_age_secs` (3600),
`min_interval_secs` (21600), and `absence_cap` (2). Startup rejects settings
that weaken the visit policy: quiet hours must wrap midnight and cover
21:00–06:59, the gap must be at least six hours, and the absence cap must be 1
or 2.

Quiet hours use each device's local time. Social notifications and summaries
suppressed at night are not queued for morning. These gates only discard pushes;
they do not create inbox entries. The client inbox depends on separate backend
notification rows, and not every category (notably bell new-post fan-out) has a
guaranteed durable row. A suppressed or expired push can therefore be the only
notification of that activity. The same one-hour age cutoff applies to later
pages and resumed jobs in a large new-post fan-out.
Campaigns retain consent checks and their existing deferred-delivery contract,
then share the same frequency budget. The explicit staff-only campaign bridge
continues to bypass consent/timezone checks, but not the frequency budget.
Campaign quiet hours use the shared 21:00–07:00 default. Any refusal by the
shared policy is terminal suppression (`recipient_frequency_capped`, or
`recipient_returned` when the final pre-send check finds an active device, a
visit, or a newer push), not a quiet-hours deferral; a campaign is not retried just to wait out
the gap or the cap.

Encrypted kind 3079 registration accepts optional `timezone` (IANA name) and
`timezoneOffsetMinutes`. Named zones take precedence at send time and handle DST
without re-registration. Offsets remain supported for existing apps. Invalid
names fall back to an offset; absent/invalid optional timezone data does not
reject a token registration. Tokens without usable timezone data receive no
social, direct-message, or campaign pushes.
The mobile app refreshes registration on resume after the zone or offset changes.
Travel while an app stays closed cannot be detected by the server.

Social events older than one hour are not newly pushed. Existing coalesced groups
retain their configured logical lifetime. All sends request immediate provider
delivery (`android.ttl=0s`, `apns-expiration=0`) rather than storing alerts for an
offline device and replaying them later. This intentionally trades offline push
catch-up for timely alerts; it does not remove existing inbox rows. Provider transport
and OS scheduling remain best-effort.

Public content pushes request iOS background warming with `content-available`
while retaining `aps.alert`; the OS owns that banner. Mobile must retain its
`RemoteMessage.notification` duplicate-render guard. Background work warms public
thumbnails in the same disk cache used by the app. Foreground receipt warms target
metadata through the existing video repository, profiles through the profile
repository, and thumbnails. Both paths are bounded, tolerate failure, skip private
messages and campaigns, and do not delay navigation or fetch video files.

## Rollout and validation

The mobile change adds a native timezone plugin and needs a normal native app
release. Existing clients can continue using offset-only registration; no token
migration is required. Deployments are separate from these code changes.

Before rollout, verify on physical iOS and Android devices: a single alert with
background warming, receipt-to-open thumbnail reuse, foreground/background/
terminated delivery, offline delivery, and changed timezone on resume. OS-denied
background execution must leave the normal notification tap path usable.
Monitor `push_delivery_policy_total` outcomes (`quiet_hours`,
`timezone_unknown`, `expired`, `missing_activity_target`, `active`,
`stale_visit`, `allowance_exhausted`, `cooldown`, `frequency_capped`) and
`push_activity_total` outcomes (`visit`, `renewed_or_stale`) alongside existing
FCM and coalescing counters. These metric labels contain no recipient
identifiers. The quiet-hours and timezone counters count device evaluations,
including the final send-time recheck, and `cooldown` counts each re-evaluation
of a requeued digest, rather than distinct notifications.

The additional Redis state is:

- `delivery_budget:{pubkey}`: a rolling set of reserved notification
  identities, scored by their latest attempt and expiring after a day without
  a reservation.
- `delivery_generations:{pubkey}`: the visit generation each of those
  identities was reserved under, with the same lifetime.
- `notification_activity:{pubkey}`: the account's visit `generation`,
  `absence_used`, and `last_activity_at`. It has no expiry.
- `notification_active:{pubkey}`: foreground sessions scored by presence
  expiry.
- `notification_activity_session:{pubkey}:{session}`: the last applied
  `sequence`, `state`, and `visit` of one app session, kept for a week.
- `notification_work_generation:{pubkey}:{hash}`: the visit generation a
  notification was first attempted under, so a retry cannot adopt a later
  visit. Written only for accounts with registered devices and kept for at
  least a week.
- `token_timezone_names`: device IANA names, removed with device tokens.
