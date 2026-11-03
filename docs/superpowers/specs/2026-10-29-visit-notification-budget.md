# Notifications between visits

Approved in the implementation conversation on 2026-10-29. Opening Divine
resets the interruption allowance. The six-hour cooldown survives a visit.

## Behavior

- Aggregate likes and reposts from the first event, across posts and types, into
  one recipient summary. Keep underlying inbox/read state independent.
- Allow at most two nonurgent pushes while away. A foreground visit resets this
  allowance and invalidates queued nudges from earlier visits. Heartbeats do not
  reset it. Any active device suppresses nonurgent pushes.
- Retain delivery identities, provider outcomes, and the minimum six-hour gap
  across visits. Resetting does not grant another send of an old identity.
- Keep consent, expiry, existing stricter campaign limits, and recipient-local
  quiet hours. Nonessential delivery is silent from 21:00 through 06:59. Unknown
  timezone suppresses nonessential pushes.
- Presence expires after 180 seconds; foreground clients renew every 60 seconds.
  A missing background event cannot suppress pushes forever.
- No app-open operation marks notifications read or deletes inbox records.

## Ownership and alternatives

Extend the existing Redis coalescer and shared push delivery policy. A mobile-only
timer cannot coordinate devices or retries. Campaign-only caps do not cover
social pushes. A new notification service is unnecessary.

Mobile sends authenticated lifecycle changes to Funnelcake; Funnelcake verifies
the account and forwards through the existing trusted push bearer boundary.
Device credentials and tokens stay exclusively in push-service. No new public
push endpoint or production resource is required by this implementation.

## Activity contract

Public POST /api/v2/notifications/activity accepts a strict JSON object:
`sessionId` (UUID), `sequence` and `visit` (integers 1..2147483647, visit <= sequence), and `state`
(`foreground` or `background`). Maximum body size is 4096 bytes. NIP-98 binds
the method and exact raw body under Funnelcake's existing URL verification rules
(including trailing-slash normalization). Its normal 60-second authentication
window and ten-second future-clock tolerance apply.

Internal POST /internal/v1/activity accepts the same object plus
`recipientPubkey`, derived only from the verified public request signer. It uses
the existing push-service internal bearer credential. Both endpoints return 204
for applied, duplicate, or stale activity; failures return a non-success status.
The gateway has a two-second timeout, no redirects, and no upstream retries.

A session identifies one account/process lifetime, across foreground visits.
Sequence orders all transitions and heartbeats within that session. An older
foreground cannot override a newer background. The visit counter increments on each real foreground transition and remains
unchanged for heartbeats. The first observation of a higher visit resets the
allowance once, including when its foreground report was lost. Duplicate or
older sequences and regressive visit counters are ignored. Heartbeats never
refill allowance, even after presence expiry.
Server receipt time is authoritative. Concurrent devices have independent
presence leases and share one account allowance. Signed request freshness limits
delayed unseen sessions; these can renew suppression but never bypass cooldown.

## Delivery races

Coalesced work carries the account visit generation. Final atomic authorization
checks its generation, active presence, expiry, allowance, and cooldown. An open
invalidates unsent work; it cannot recall a provider request already submitted.
An accepted or ambiguous attempt retains its identity and spent allowance.
Activity never erases historical reservations. Old worker groups are fenced by
the same current policy before sending.

The first recipient attempt durably binds its work identity to a generation
before fallible eligibility lookups. A later retry cannot acquire a newer visit
after an earlier preflight failure. Ingest rejects a changed generation before
creating group state. Retry authorization preserves its allowance but respects
other notifications' cooldowns and refreshes its attempt timestamp for later
sends. The final provider preflight repeats that cooldown check.

## Rollout dependencies

Deploy the activity receiver and gateway before enabling mobile lifecycle
reporting. Ship mobile support for `activityDigest` before enabling the new
buffer-only digest configuration, so taps reach Notifications on supported
clients. No production resources or deployments are included in this change.

Passive lifecycle requests must never open a signing approval UI. Amber uses
its noninteractive content resolver; unsupported interactive signers fail
closed. Activity failures do not block app startup or navigation.

## Verification

Exercise burst aggregation across posts/types, duplicate events, stale activity,
multiple devices, heartbeat expiry, concurrent sends, cooldown after reset, old
queued jobs, unavailable Redis/upstream, account switch, signature/body binding,
and quiet-hour/unknown-timezone suppression. Use synthetic identifiers only.
Implementation authorization does not authorize production deployment.
