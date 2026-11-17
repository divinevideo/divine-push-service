# Launch marketing push campaigns

Approved by Rabble on 2026-11-17. This is the implementation record for the
approved cross-repository launch, not authorization to send a public campaign.

## Goal

Make the existing campaign dashboard usable end to end: choose an audience,
write and preview a message, approve it, schedule delivery, and inspect results.
Reuse divine-engagement and the existing push collector.

## Task 1: Campaign workspace

- Load enabled audience choices from the segment catalog, including opted-in
  users, staff tests and explicit recipient lists. Show estimates/holdouts and
  hide pasted recipients where unnecessary.
- Replace obsolete disabled-delivery copy with actual production and pause
  state, roster count and snapshot time. Refuse roster-based scheduling with a
  missing snapshot or a snapshot older than 45 minutes.
- Validate Following, video and creator-profile targets server-side. Do not
  offer unsupported HTTPS targets.
- Default production campaigns to 10% holdout and expiry 48 hours after their
  scheduled start. Enforce expiry and a nonzero holdout server-side.
- Page materialization and queue fan-out up to the existing 200,000-recipient
  bound. Keep Workflow results bounded and retries idempotent.
- Distinguish pending, deferred, suppressed, failed and provider acceptance.
- Test campaign lifecycle, policy, roster freshness, targets, fan-out and UI.

## Task 2: Compatible device delivery

- Add optional supported campaign target types to encrypted registration.
  Missing capability means Following-only. Store capabilities per token and
  remove them with the token.
- Send video/profile campaigns only to compatible devices; report an explicit
  suppression when none exists.
- Preserve lease identifiers, deduplication, consent, quiet hours and the
  shared interruption budget. Test mixed old/new devices and retry behavior.

## Task 3: Mobile destinations and measurement

- Advertise supported targets and route campaign taps to Following, a video or
  creator profile through existing navigation across all app states.
- Preserve revision IDs through normalized/local tap payloads. Record opens
  through the existing analytics sink only with analytics consent.
- Use existing consented analytics for completed playback in 24 hours and
  return activity in seven days against holdouts; include coverage and opt-outs.
- Test malformed targets, account switching, foreground/background/cold starts,
  consent and duplicate rendering.

## Operating defaults and rollout

Start with one fresh campaign daily, alternating discovery, videos and useful
product prompts. Keep the six-hour gap, two nonurgent pushes between visits,
rolling fallback, quiet hours and revocable marketing permission. Do not use
time-sensitive marketing alerts. Behavioral automation/new targeting models
are out of scope.

Run affected Rust, engagement and Flutter checks. Deploy backend compatibility
before mobile support, then expose video/profile creation. Verify on physical
iOS and Android devices, then stage at 100 recipients, 1,000 recipients and the
broader opted-in audience, inspecting results at each stage. Production sends
and deployments follow their owning repository's authorization workflow.

Success: a marketer can schedule without engineering assistance; eligible
devices receive one correct alert; taps reach the promised destination; and
delivery outcomes are visible. Provider acceptance is not device display/open.
