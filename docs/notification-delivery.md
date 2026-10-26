# Notification delivery policy

Social pushes, coalesced summaries, and campaigns share a per-recipient rolling
budget: five distinct notifications per hour and twenty per day. Reservations
are atomic in Redis across replicas and device tokens. Retries reuse their
identity; failed or ambiguous provider attempts conservatively keep a slot.
Existing like/repost grouping remains in place, with one immediate alert per
post/type/two-hour bucket before a summary.

`service.delivery` accepts `quiet_start_hour` (22), `quiet_end_hour` (8),
`hourly_cap` (5), `daily_cap` (20), and `max_event_age_secs` (3600).
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
Campaign quiet hours now use the shared 22:00–08:00 default, replacing their
previous 21:00–07:00 window. A frequency-cap refusal is terminal suppression,
not a quiet-hours deferral; a campaign is not retried just to wait out the cap.

Encrypted kind 3079 registration accepts optional `timezone` (IANA name) and
`timezoneOffsetMinutes`. Named zones take precedence at send time and handle DST
without re-registration. Offsets remain supported for existing apps. Invalid
names fall back to an offset; absent/invalid optional timezone data does not
reject a token registration. Social delivery remains enabled for legacy tokens
without timezone data; campaigns still suppress unknown-timezone recipients.
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
`legacy_timezone_unknown`, `frequency_capped`, `expired`) alongside existing FCM
and coalescing counters. These metric labels contain no recipient identifiers.
The quiet-hours and legacy-timezone counters count device evaluations, including
the final send-time recheck, rather than distinct notifications.

The additional Redis state is `delivery_budget:{pubkey}` (a rolling set of
reserved notification identities, expiring after a day without a reservation)
and `token_timezone_names` (device IANA names, removed with device tokens).
