# Post Live Activity: push service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep a creator's post Live Activity on iPhone current for the post's first hour: the push service hears about the activity through kind 3079, checks Funnelcake every ~3 minutes, and sends silent Live Activity updates through Firebase, ending with a summary.

**Architecture:**
- **New `live_activity` modules** sit beside the existing pipeline.
  - A 3079 that carries a `liveActivity` object is routed to them and never touches device registration.
  - Activities live in their own Redis keys. A scored job set drives a worker that batches Funnelcake lookups and sends through a new Firebase method that bypasses the interruption policy.
  - Everything is behind `live_activity.enabled` (default off), except the rule that a Live Activity registration never registers a notification token. That rule holds even with the flag off.
- **Three small mobile tasks** (Part B) make the widget read standard Unix-seconds dates, put the shared timings in one place, and dim the Lock Screen counts while the card is stale.
- **One contract fixture file** is run by both repos so their numbers cannot drift.

**Tech Stack:** Rust (tokio, deadpool-redis Lua scripts, reqwest 0.12, serde, axum test stubs); Swift (ActivityKit `Codable`); Dart (Flutter tests).

**Spec:** `divine-mobile` `docs/superpowers/specs/2026-11-18-post-live-activity-design.md`, sections *Push service*, *When the server is told an activity ended*, *Infrastructure*. Read both.

## Decisions already made (Matt, 2026-11-20)

- Extend kind 3079's encrypted payload. No new kinds and no HTTP endpoint.
- The widget decodes `updatedAt` as Unix seconds (Part B, Task M1). The server sends Unix seconds everywhere.
- Firebase only for v1. Direct-to-Apple stays a named fallback, gated on the device test showing that a phone with notifications declined gets no Firebase token. Evidence that it will get one: the app's Firebase plugin calls `registerForRemoteNotifications` at launch regardless of permission (`FLTFirebaseMessagingPlugin.m:416-422`, auto-init on), the app declares `remote-notification` background mode, and Apple says a token does not need authorization.
- Matt works through the access steps (Firebase console, staging send, deploys) with Daniel.
- Zero views: the server matches the app. Funnelcake reports zero views as `views: null` (`video_stats_from_row`, funnelcake `crates/clickhouse/src/client.rs:928-933`), and the app's stats screen asks `GET /api/videos/{id}/views` for those posts. The server does the same lookup, and handles 429 and 503 properly. Separately, ask the Funnelcake maintainers whether "null means 0 for a found post" is guaranteed, which could later let both sides drop the extra lookup.
- An outage across the hour's end: the server ends the card with the last content it actually sent and asks iOS to remove it at once, rather than leaving a "live" card for up to ~11 hours.
- A stale card dims its Lock Screen counts now (Task M3), and Sebastian reviews the look on the app PR.
- Execution: subagent-driven.
- Daniel reviews and may improve or redirect any of this.

## Engineering rulings in this plan (each one a reviewer can reject)

- **A Live Activity payload skips `apply_device_registration` entirely.** That keeps the tokens of people who declined notifications out of `user_tokens:*` by construction, so they can never receive alert or background pushes. The app registers authorized devices for notifications through its own path (session ready, token refresh, timezone change: `push_notification_session_coordinator.dart:113-273`), and returns early when permission is denied. Routing authorized payloads through registration as well would be harmless but redundant, because the payload carries the same metadata as a normal registration (`push_notification_service.dart:504-510`). *Cost if wrong:* none found. Cross-checked 2026-11-20.
- **A separate send path, not a third `DeliveryClass`.** The policy lives only at its call sites (`event_handler.rs:2063-2101`, `campaign_delivery.rs:591,615`, `coalesce.rs:1347,1459,1536,1595`), and the transport applies none. A class would have to opt out of every line of the shared reservation script (presence `notification_delivery.rs:186`, generation `:185`, hour and day caps `:223-224`). If its reservations were counted, it would spend the creator's own 5/hour and 20/day budget. Note in `docs/notification-delivery.md` that Live Activity is deliberately not a class, and correct "all categories" at `notification_delivery.rs:75`. *Cost if wrong:* a later refactor, if Daniel prefers a class.
- **When Funnelcake fails, the worker skips the send and reschedules, with jitter.** A blip under ~10 minutes never shows, because the next success refreshes the stale date first. A longer outage shows "Updated … ago" honestly. Sending would refresh the stale date and claim a check-in that did not happen. At the hour's end with stats still failing, the worker sends an `end` carrying the last content it actually sent, with a dismissal date in the past, so iOS removes the card at once. It sends no false summary.
- **The Funnelcake URL may use `http://` only for in-cluster or loopback hosts**, and `https://` otherwise. It must be origin-only. This matches the existing in-cluster pattern between these same services (funnelcake-api → `http://divine-push-service.push.svc.cluster.local:8000`, divine-iac-coreconfig `k8s/applications/funnelcake-api/overlays/production/kustomization.yaml:294-295`), meshed by Linkerd. The campaign validator is stricter because that client sends Access credentials. This one sends none.
- **Only a post's author can start its card:** the `postCoordinate` pubkey must equal the event's signer. Each owner may have at most 5 active activities, because each registration creates an hour of recurring work.
- **The worker must never take the service down:** each activity's processing runs under `catch_unwind`, a tick error is logged and counted and the loop continues, a tick finishes before its lease runs out, and the worker never touches `user_tokens:*` or `token_to_pubkey`.
- **No Apple push key and no `apns-topic`.** Firebase sets the topic for `live_activity_token` messages.

## Global Constraints

- Feature flag `live_activity.enabled` defaults to `false` in `config/settings.yaml` and `config/settings.development.yaml`. Staging runs with `APP_ENV=production`, so it reads `settings.yaml`. Disabled: no worker is spawned and no active activity is stored or sent. An `ended` registration is still processed (tombstoned), so re-enabling never resumes an activity the person ended.
- **Rollout order:** the server routing deploys and finishes rolling out in an environment before any app build with `FeatureFlag.postLiveActivity` enabled reaches people there. Until then, today's server parses a Live Activity 3079 as a normal registration, and a declined-notification phone would stick in `user_tokens:*` (cleanup never catches it, because each delivered push refreshes its `stale_tokens` score).
- **Wire payload** (from the app, inside the decrypted 3079 JSON, beside `token`):
  - `notificationsAuthorized`: bool.
  - `liveActivity`: `{ activityPushToken: hex string, postCoordinate: "<kind>:<64 hex>:<d>", postEventIds: [64-hex, ...], postedAt: unix secs, windowEndsAt: unix secs, showFirstRunNote: bool, state: "active" | "ended" }`.
- **Content state the server sends**, keys exact: `phase` ("posting"|"live"|"notFound"|"summary"), `views`, `likes`, `comments` (int or null), `updatedAt` (Unix seconds, integer), `showFirstRunNote` (bool), `postCoordinate` (string), `postEventId` (string, `postEventIds[0]`). Every content state echoes `postCoordinate`, `postEventIds[0]` and `showFirstRunNote`.
- **Timings:**
  - cadence 180 s;
  - settle point `postedAt` + 600 s;
  - window = the payload's `windowEndsAt` (the app sends `postedAt` + 3600);
  - `stale-date` = send time + 600 s;
  - `dismissal-date` = end + 1800 s on the end event.
- **Stats mapping:** `views` → `views`, `reactions` → `likes`, `comments` → `comments`, per id from `POST /api/videos/stats/bulk` (`{event_ids, window?}` → `{stats: {id: {views: u64|null, reactions: u64, comments: u64, ...}}, missing: [id]}`, at most 100 ids per call).
  - Across an edited post's ids, take each counter's maximum, treating null as unknown.
  - Bulk reports zero views as `views: null`. If bulk gives no views for any of the post's ids, fall back to `GET /api/videos/{id}/views` → `{views: u64, ...}` for `postEventIds[0]`, as the app does. A failed fallback is treated as a failed stats request (skip), never as unknown.
  - Before the settle point, send any 0 as null. From the settle point on, a 0 is a confirmed 0.
- **Can't find:** at or after `postedAt` + 600 s, every id must be in `missing` or absent from `stats` in *two consecutive successful* checks (stored as `absent_checks` on the activity) before the worker sends `end` with phase `notFound` and the 1800 s dismissal. This guards against one lagging read. A failed request is never absence and never resets the count. `postedAt` is the device time at publish success (`post_live_activity_repository.dart:152`). For an activity adopted after a cold start it is approximated by `startedAt`, which is earlier and therefore conservative (`:466-476`).
- **Firebase message:** `{token: <FCM token>, apns: {live_activity_token: <activityPushToken>, headers: {"apns-priority": "5" for update, "10" for end}, payload: {aps: {timestamp: <unix secs>, event: "update"|"end", "content-state": {...}, "stale-date": <unix secs>, "dismissal-date": <unix secs, end only>}}}}`, plus header `"apns-expiration": "<stale-date>"`, so a late update is dropped rather than delivered stale. No `notification` and no `data`.
- **Rules:**
  - Ending is a 3079 with `state: "ended"`, matched by `activityPushToken` and the same owner pubkey. It never creates or refreshes a notification registration.
  - One active activity per owner and FCM token: a new one supersedes the previous one, which stops being updated.
  - A 3080 never ends an activity.
  - Preferences (3083), quiet hours and caps do not apply.
  - The worker never prunes `user_tokens:*` or `token_to_pubkey`, even on `TokenNotRegistered`. It finishes the activity only.
  - Live Activity sends do not touch the shared `fcm_send_*` metrics (`metrics.rs:104-113`), only their own.
- **Logging:** full event ids and pubkeys, and `token_prefix` (8 characters) for FCM and activity tokens. Never log a full token.
- **Style** (`AGENTS.md`): `ServiceError` with thiserror, `tracing`, `///` on public items, one module per responsibility. New Redis keys go in both Redis tables (`AGENTS.md`, `docs/developer-guide.md`).

## Review Focus

1. **Startup replay** of a week of 3079s must not resurrect an activity that already ended or whose window has passed. Test in Task 5: `replayed_active_after_ended_is_ignored`, `expired_window_is_ignored`.
2. **Funnelcake down or slow** must not produce "can't find" and must not send. Test in Task 9: `stats_failure_skips_send_and_reschedules`.
3. **A lease expiring mid-send** means two workers may handle one job. Doubles must be harmless, and a second end after cleanup a no-op. Test in Task 9: `end_twice_is_noop`.
4. **A payload with `notificationsAuthorized: false`** must never land in `user_tokens:*`, even with the feature flag off. Test in Task 5: `live_activity_payload_never_registers_token_when_disabled`.
5. **A malformed or hostile `liveActivity`** (bad hex, empty or more than 10 `postEventIds`, a window longer than 3600 s, `windowEndsAt` beyond now + 3600 s, a coordinate naming another author, an ended message from another pubkey, `"liveActivity": null`) must be rejected without touching anything. Tests in Tasks 3 and 5.
6. **A worker panic or tick error** must not stop the service or the worker. Test in Task 9: `panic_in_one_activity_is_contained`, `tick_error_keeps_loop_running`.
7. **An outage spanning the hour's end** must remove the card, not leave it. Test in Task 9: `outage_at_window_end_ends_with_last_content_and_immediate_dismissal`.

---

## File Structure

| File | Responsibility |
|---|---|
| `tests/fixtures/live_activity_contract.json` | Shared contract cases (also copied into divine-mobile) |
| `src/live_activity/mod.rs` | Module root; re-exports |
| `src/live_activity/payload.rs` | Parse and validate the `liveActivity` and `notificationsAuthorized` wire fields |
| `src/live_activity/store.rs` | Redis keys, Lua scripts: upsert, end, supersede, claim due jobs, reschedule, delete |
| `src/live_activity/content.rs` | Pure rules: stats → content state, phase, "can't find" |
| `src/live_activity/stats_client.rs` | Funnelcake bulk stats and views fallback |
| `src/live_activity/worker.rs` | The poll loop: claim, fetch, decide, send, reschedule or clean up |
| `src/config.rs` | `LiveActivitySettings` |
| `src/crypto.rs` | `TokenPayload` gains two raw optional fields |
| `src/event_handler.rs` | Route a `liveActivity` 3079 to the new module |
| `src/fcm_sender.rs` | `FcmSend::send_live_activity` plus the real and mock implementations |
| `src/main.rs` | Spawn the worker when enabled |
| `src/metrics.rs` | Counters |
| docs + `AGENTS.md` + `config/*.yaml` | Protocol, delivery class, payload, Redis keys, flag |

---

## Part A: divine-push-service

### Task 1: Contract fixture and settings

**Files:**
- Create: `tests/fixtures/live_activity_contract.json`, `tests/live_activity_contract_test.rs`
- Modify: `src/config.rs` (add `LiveActivitySettings`, field on `Settings` with `#[serde(default)]`, validation), `config/settings.yaml`, `config/settings.development.yaml`

**Interfaces:**
- Produces:
  - `pub struct LiveActivitySettings { pub enabled: bool, pub funnelcake_base_url: String, pub cadence_secs: u64, pub poll_millis: u64, pub request_timeout_secs: u64, pub lease_secs: u64, pub tick_deadline_secs: u64, pub max_concurrency: usize, pub max_active_per_owner: usize }`, with defaults `false, "", 180, 1000, 10, 120, 90, 8, 5`. `validate()` requires `tick_deadline_secs < lease_secs` and `max_concurrency` from 1 to 8, which keeps it under the 10-connection Redis pool (`config/settings.yaml:63`).
  - `pub fn validate_funnelcake_base_url(url: &str) -> Result<(), String>`.
  - The fixture's schema below.

Fixture schema (top level):
- `version: 1`.
- `timings: {settleSeconds: 600, windowSeconds: 3600, cadenceSeconds: 180, staleSeconds: 600, summaryDismissalSeconds: 1800}`.
- `counts: [{name, secondsSincePosted, response: "ok"|"failed", versions: [{views: int|null, reactions: int, comments: int} | "missing"], viewsFallback: int|"failed"|null, absentChecksBefore: int (default 0), expected: {action: "update"|"end"|"skip", phase, views, likes, comments}}]`.
- `contentStates: [{name, json: {...content state...}, expected: {phase, views, likes, comments, updatedAt, showFirstRunNote, postCoordinate, postEventId}}]`.
- `links: [{postEventId, postCoordinate, url}]`.

- [ ] **Step 1: Write the fixture** with at least these `counts` cases. Values must match Global Constraints.
  - `zero_before_settle_is_unknown` (240 s, one version {0,0,0} → update, live, null, null, null)
  - `zero_after_settle_is_confirmed` (720 s, {0,0,0} → update, live, 0, 0, 0)
  - `missing_views_uses_fallback` (720 s, {null,3,1}, fallback 12 → 12, 3, 1)
  - `zero_views_from_fallback_after_settle` (720 s, {null,3,1}, fallback 0 → 0, 3, 1)
  - `zero_views_from_fallback_before_settle` (300 s, {null,0,0}, fallback 0 → update, live, null, null, null)
  - `max_across_versions` (720 s, [{5,2,0},{9,1,4}] → 9, 2, 4)
  - `absent_before_settle_keeps_waiting` (300 s, ["missing"] → update, live, null, null, null)
  - `absent_after_settle_first_miss_waits` (660 s, ["missing"], `absentChecksBefore: 0` → update, live, null, null, null)
  - `absent_after_settle_second_miss_is_not_found` (840 s, ["missing"], `absentChecksBefore: 1` → end, notFound)
  - `failed_request_skips` (900 s, response failed → skip)
  - `failed_fallback_skips` (720 s, {null,3,1}, fallback "failed" → skip)
  - `window_end_is_summary` (3600 s, {40,5,2} → end, summary, 40, 5, 2)

  Add two `contentStates` cases: one full, and one with `postCoordinate` and `postEventId` absent, which must decode with both null. `updatedAt` is integer Unix seconds, for example `1763500000`. Add three `links` cases, including a d-tag containing `& = + ? /` and a space. Each `url` is `divine:///post-analytics/<postEventId>?coordinate=<postCoordinate percent-encoded with URLQueryAllowed minus `&=+?/`>`, the rule in divine-mobile's `PostActivityLink.url` (`ios/Shared/PostLiveActivity/PostActivityDisplay.swift`). Task M1's `testLinksMatchFixture` checks every value against that function, so a hand-encoding mistake fails there.
- [ ] **Step 2: Write `tests/live_activity_contract_test.rs::fixture_checksum_is_pinned`.** It asserts that the sha256 of the fixture bytes equals `const CONTRACT_SHA256: &str = "<hex>"`, so an edit must touch both repos' pinned constants on purpose. Also add `fixture_parses`, which deserializes it into test-local structs.
- [ ] **Step 3: Write `config` tests:**
  - `live_activity_defaults_off`: `Settings` from the development file has `enabled == false`.
  - `funnelcake_url_validation`:
    - accepts `http://funnelcake-api.funnelcake.svc.cluster.local:8080`, `http://127.0.0.1:4000` and `https://relay.divine.video`;
    - rejects `""`, `ftp://x`, `https://user:pw@x`, `http://relay.divine.video` (plaintext to a public host), `https://relay.divine.video/api` (a path) and `https://relay.divine.video?x=1` (a query).
    - `http` is allowed only for hosts ending in `.svc.cluster.local` or `.svc`, and for `localhost`, `127.0.0.1` and `::1`.
  - `enabled_requires_url`: `validate()` errors when enabled with an empty URL.
- [ ] **Step 4: Run** `cargo test --test live_activity_contract_test` and `cargo test config`. Expect FAIL (missing items).
- [ ] **Step 5: Implement** the settings, defaults and validation (mirror `CampaignDeliverySettings`, `config.rs:44-99`). Add a `live_activity:` block with `enabled: false` to both YAML files.
- [ ] **Step 6: Run** the same tests and expect PASS. Then `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] **Step 7: Commit** `feat(live-activity): add the shared contract fixture and settings, off by default`.

### Task 2: Firebase Live Activity send

**Files:**
- Modify: `src/fcm_sender.rs` (trait method, `RealFcmClient`, `FcmClient`, `MockFcmSender`)
- Test: inline tests in `src/fcm_sender.rs`, using the existing `stub_fcm` pattern (~`:1260-1334`)

**Interfaces:**
- Produces:
  - `pub enum LiveActivityEvent { Update, End { dismissal_unix: u64 } }`.
  - `pub struct LiveActivityMessage { pub fcm_token: String, pub activity_token: String, pub event: LiveActivityEvent, pub content_state: serde_json::Value, pub timestamp_unix: u64, pub stale_unix: u64 }`.
  - On trait `FcmSend`: `async fn send_live_activity(&self, message: &LiveActivityMessage) -> Result<(), FcmError>`. The default impl returns `Err(FcmError::InternalRequest("live activity unsupported".into()))`, so other test fakes still compile.
  - On `FcmClient`: `pub async fn send_live_activity(&self, message: &LiveActivityMessage) -> Result<(), FcmError>`.
  - `pub fn live_activity_body(message: &LiveActivityMessage) -> serde_json::Value`, the pure builder.

- [ ] **Step 1: Write the failing tests:**
  - `live_activity_update_body_shape` asserts `live_activity_body` equals the exact JSON in Global Constraints for an update: priority "5", `stale-date` present, `apns-expiration` equal to it, no `dismissal-date`, and none of `notification`, `data`, `android`.
  - `live_activity_end_body_has_dismissal_and_priority_10`.
  - `real_client_posts_live_activity_body` stubs Firebase and asserts that the captured request body's `message` equals `live_activity_body`.
  - `mock_records_live_activity`.
  - `live_activity_send_times_out`: a stub that never answers ends in a timeout error within `FCM_OPERATION_TIMEOUT`.
  - `live_activity_send_leaves_shared_fcm_metrics_untouched`.
  - `live_activity_outcome_mapping`, for `pub fn live_activity_send_outcome(err: &FcmError) -> SendOutcome` with `pub enum SendOutcome { Finish, RetryAfter(Duration), RetryNextCycle }`:
    - `TokenNotRegistered` → `Finish`
    - `InvalidRequest` → `Finish` (a bad payload will not improve by retrying)
    - `Unauthorized` → `RetryNextCycle` (a missing APNs key needs ops, not a retry storm)
    - `RetryableInternal(d)` → `RetryAfter(d)`
    - other errors → `RetryAfter(60 s)`
- [ ] **Step 2: Run** `cargo test live_activity_` and expect FAIL.
- [ ] **Step 3: Implement.** `RealFcmClient::send_live_activity` reuses the token provider, URL and `classify_error`. It is wrapped in the same `FCM_OPERATION_TIMEOUT` as `send_single` (`fcm_sender.rs:467`), because `gcp_auth` token fetches have no timeout of their own. It logs only `token_prefix` of both tokens and does not call `build_apns_config`. The trait's default method logs at `error!` when it is hit, so a wrapper that forgets to forward it is loud. Capture the exact Firebase error body for a rejected `live_activity_token` during the staging test and refine the mapping then.
- [ ] **Step 4: Run** and expect PASS, plus `cargo test fcm_sender` to show no regression.
- [ ] **Step 5: Commit** `feat(live-activity): send Live Activity updates through Firebase`.

### Task 3: Payload parsing and validation

**Files:**
- Modify: `src/crypto.rs` (`TokenPayload`)
- Create: `src/live_activity/mod.rs`, `src/live_activity/payload.rs`; add `pub mod live_activity;` in `src/lib.rs`

**Interfaces:**
- Produces:
  - Two new `TokenPayload` fields: `#[serde(rename = "notificationsAuthorized", default)] pub notifications_authorized: Option<bool>` and `#[serde(rename = "liveActivity", default)] pub live_activity: Option<serde_json::Value>`. Kept raw so that a present-but-invalid object is distinguishable from an absent one.
  - `pub enum ActivityState { Active, Ended }`.
  - `pub struct LiveActivityRegistration { pub activity_token: String, pub post_coordinate: String, pub post_event_ids: Vec<String>, pub posted_at: u64, pub window_ends_at: u64, pub show_first_run_note: bool, pub state: ActivityState }`.
  - `pub fn parse(raw: &serde_json::Value, signer_hex: &str, now_unix: u64) -> Result<LiveActivityRegistration, PayloadError>`.
  - `pub fn is_live_activity_payload(payload: &TokenPayload, raw_json: &serde_json::Value) -> bool`: true when the decrypted JSON *has the key* `liveActivity` (even if `null`) or has `notificationsAuthorized`. Routing uses this, so `"liveActivity": null` can never fall through to normal registration.
  - `pub enum PayloadError` (thiserror), one variant per rule.

Validation rules:
- `activityPushToken` is lowercase hex, 32 to 512 characters.
- `postCoordinate` matches `^\d+:[0-9a-f]{64}:.+$`, and its pubkey part equals the signer, passed as `signer_hex` (see the signature below).
- `postEventIds` has 1 to 10 entries, each 64 lowercase hex characters.
- `0 < windowEndsAt - postedAt <= 3600`.
- `windowEndsAt <= now + 3600`.
- `state` is "active" or "ended".

- [ ] **Step 1: Write failing tests:**
  - `parses_app_wire_shape`, using a JSON literal byte-for-byte as divine-mobile's `LiveActivityRegistration.liveActivityWire()` produces it.
  - `token_payload_keeps_unknown_live_activity_raw`.
  - One test per rule: `rejects_bad_token_hex`, `rejects_bad_coordinate`, `rejects_coordinate_of_another_author`, `rejects_empty_event_ids`, `rejects_eleven_event_ids`, `rejects_window_over_an_hour`, `rejects_window_far_future`, `rejects_unknown_state`.
  - `null_live_activity_is_still_a_live_activity_payload`.
- [ ] **Step 2: Run** `cargo test live_activity::payload` and expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** and expect PASS, plus `cargo test crypto` to show no regression.
- [ ] **Step 5: Commit** `feat(live-activity): parse and validate the Live Activity registration payload`.

### Task 4: Activity store

**Files:**
- Create: `src/live_activity/store.rs`
- Test: `tests/live_activity_store_test.rs` (real Redis; skip when unavailable, like `tests/roster_publisher_test.rs:9-24`)

**Interfaces:**
- Produces:
  - Keys:
    - `live_activity:{blake3(activity_token)}`, a hash with fields `owner`, `fcm_token`, `activity_token`, `post_coordinate`, `post_event_ids` (JSON), `posted_at`, `window_ends_at`, `show_first_run_note`, `last_at` (20-digit `created_at`), `last_id`, `state`, `absent_checks`, `last_content` (the JSON content state of the last successful send), `last_sent_at`. TTL is `window_ends_at` + 1800 + 300 s from now.
    - `live_activity_owner:{owner}`, a set of active activity ids, used for the per-owner cap. TTL refreshed with each member.
    - `live_activity_device:{owner}:{blake3(fcm_token)}`, which holds the current activity id.
    - `live_activity_jobs`, a sorted set: member is the activity id (the blake3 hex), score is the due time in Unix ms.
  - `pub struct StoredActivity { pub id: String, pub owner: String, pub fcm_token: String, pub registration: LiveActivityRegistration }`.
  - `pub enum UpsertOutcome { Created, Updated, Superseded { previous_id: String }, Stale, Tombstoned, OverOwnerCap }`.
  - `pub async fn upsert_active(pool: &RedisPool, owner: &str, fcm_token: &str, reg: &LiveActivityRegistration, event_at: u64, event_id: &str, first_due_ms: u64) -> Result<UpsertOutcome>`.
  - `pub async fn end_by_token(pool: &RedisPool, owner: &str, activity_token: &str, event_at: u64, event_id: &str) -> Result<EndOutcome>`, where `pub enum EndOutcome { Ended, NotFound, NotOwner, Stale }`.
  - `pub async fn claim_due(pool: &RedisPool, now_ms: u64, lease_ms: u64, limit: usize) -> Result<Vec<StoredActivity>>`.
  - `pub async fn reschedule(pool: &RedisPool, id: &str, due_ms: u64) -> Result<bool>`. Conditional: it re-adds only if the hash exists and is not tombstoned, so a slow worker cannot revive a job another worker finished.
  - `pub async fn record_sent(pool: &RedisPool, id: &str, content_json: &str, sent_at: u64) -> Result<()>`.
  - `pub async fn set_absent_checks(pool: &RedisPool, id: &str, count: u32) -> Result<()>`.
  - `pub async fn is_live(pool: &RedisPool, id: &str) -> Result<bool>`: exists and not tombstoned, checked right before any end send.
  - `pub async fn finish(pool: &RedisPool, id: &str) -> Result<()>`.

Store behaviour:
- Each operation is one atomic Lua script.
- Ordering per activity is by (`last_at`, `last_id`), the lower id winning a tie, matching NIP-01.
- An ended or finished activity leaves `state = ended` (a tombstone) until its TTL, so a replayed active message returns `Tombstoned` and changes nothing.
- An update to the same activity token merges `post_event_ids` (union, order kept, capped at 10). It does not reset the schedule.
- `claim_due` rescores members to `now + lease` (lease-by-rescore, like `claim_fanout_job`, `redis_store.rs:927-949`) and drops members whose hash is gone or tombstoned.

- [ ] **Step 1: Write failing tests:**
  - `upsert_creates_and_schedules`
  - `same_token_update_merges_ids_keeps_schedule`
  - `new_token_same_device_supersedes`: the old one is finished and its job removed.
  - `older_event_is_stale`
  - `end_by_token_owner_check` (NotOwner leaves it running)
  - `replayed_active_after_end_is_tombstoned`
  - `claim_due_leases_and_skips_finished`
  - `finish_removes_job_and_hash_state`
  - `never_touches_user_tokens`: after every operation, `user_tokens:{owner}` and `token_to_pubkey` are unchanged.
  - `sixth_active_activity_for_owner_is_refused` (OverOwnerCap, with `max_active_per_owner` 5)
  - `reschedule_after_finish_does_not_revive`
  - `record_sent_round_trips_last_content`
- [ ] **Step 2: Run** `cargo test --test live_activity_store_test` and expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** and expect PASS.
- [ ] **Step 5: Commit** `feat(live-activity): store activities and their schedule apart from device registration`.

### Task 5: Route Live Activity registrations

**Files:**
- Modify: `src/event_handler.rs` (`handle_registration`, ~`:273-367`)
- Create: `src/live_activity/registration.rs`, which owns `pub async fn handle(state: &AppState, event: &Event, fcm_token: &str, raw: &serde_json::Value) -> Result<()>`
- Test: inline tests next to the existing `signed_device_control` registration tests (~`:2759-3500`)

**Interfaces:**
- Consumes: Task 3 `parse`, Task 4 `upsert_active`, `end_by_token`, `LiveActivitySettings`.
- Produces: the routing rule. In `handle_registration`, once the payload is decrypted and the FCM token is non-empty: if `is_live_activity_payload(...)`, call `live_activity::registration::handle` and return. Never call `apply_device_registration` for it. This needs the decrypted JSON as a `serde_json::Value` alongside `TokenPayload`, so add `decrypt_token_payload_json` to `CryptoService`, or parse the plaintext twice.

`handle` behaviour:
- Reject an event outside the time window (same bounds as `apply_ordered_device_control`, `redis_store.rs:433-438`).
- When `enabled` is false, still process `Ended` (tombstone it), but store nothing for `Active`.
- On a `PayloadError`, log a warning (event id, pubkey, reason) and return `Ok`.
- `Active` with `window_ends_at <= now`: ignore.
- `Active`: `upsert_active` with `first_due_ms = now + cadence`.
- `Ended`: `end_by_token`.

- [ ] **Step 1: Write failing tests:**
  - `live_activity_payload_never_registers_token_when_disabled`: with `notificationsAuthorized` false and true, the flag off, `user_tokens` stays empty and nothing is stored.
  - `live_activity_payload_never_registers_token_when_enabled`
  - `active_registration_stores_and_schedules`
  - `ended_registration_from_other_pubkey_is_ignored`
  - `invalid_live_activity_is_dropped_not_registered`
  - `replayed_active_after_ended_is_ignored`
  - `expired_window_is_ignored`
  - `plain_registration_unchanged`: a 3079 without `liveActivity` still goes through `apply_device_registration`.
  - `null_live_activity_never_registers_token`
  - `ended_is_processed_while_disabled`
  - `coordinate_of_another_author_is_dropped`
- [ ] **Step 2: Run** `cargo test live_activity` and expect the new tests to FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** and expect PASS, plus `cargo test event_handler` to show no regression.
- [ ] **Step 5: Commit** `feat(live-activity): route Live Activity registrations away from device registration`.

### Task 6: Funnelcake stats client

**Files:**
- Create: `src/live_activity/stats_client.rs`
- Test: inline, using an axum stub on `127.0.0.1:0` (pattern: `campaign_delivery.rs:2792-2870`)

**Interfaces:**
- Produces:
  - `pub struct VersionStats { pub views: Option<u64>, pub reactions: u64, pub comments: u64 }`.
  - `pub struct BulkStats { pub stats: HashMap<String, VersionStats>, pub missing: HashSet<String> }`.
  - `pub enum StatsError { Request(String), Status(u16), RateLimited { retry_after: Option<Duration> }, Decode(String) }`. A 429 or 503 maps to `RateLimited`, reading `Retry-After` when present. Other non-2xx responses log their status.
  - `pub struct StatsClient`, constructed with `pub fn new(settings: &LiveActivitySettings) -> Result<Self, StatsError>`.
  - `pub async fn bulk_grouped(&self, groups: &[Vec<String>]) -> Vec<Result<BulkStats, StatsError>>`. Each group is one activity's ids. Whole groups are packed into ≤100-id requests, and each group's result is that request's result, so one failing chunk only skips the activities in it. Chunks run concurrently, capped at `max_concurrency`.
  - `pub async fn views(&self, id: &str) -> Result<Option<u64>, StatsError>`.
- Decode only the fields named above and ignore the rest (the response also carries `audio_reuse_*` and many stats).

- [ ] **Step 1: Write failing tests:**
  - `bulk_decodes_stats_and_missing`, with a stub returning a realistic body that includes extra fields.
  - `bulk_packs_whole_groups_under_100`: groups of 60, 30 and 20 ids give two requests, and no group is split.
  - `one_failing_chunk_only_fails_its_groups`
  - `rate_limited_reads_retry_after`
  - `bulk_failure_is_error_not_missing`: a 500 is `Err(Status(500))`.
  - `views_fallback_decodes`.
  - `timeout_is_request_error`, with the stub sleeping past `request_timeout_secs`.
- [ ] **Step 2: Run** `cargo test stats_client` and expect FAIL.
- [ ] **Step 3: Implement** with `reqwest`: redirects disabled, timeout from settings, `Content-Type: application/json`.
- [ ] **Step 4: Run** and expect PASS.
- [ ] **Step 5: Commit** `feat(live-activity): read post stats from Funnelcake`.

### Task 7: Content rules

**Files:**
- Create: `src/live_activity/content.rs`
- Test: inline unit tests, plus `tests/live_activity_contract_test.rs::count_cases` driven by the fixture

**Interfaces:**
- Consumes: `StoredActivity`, `BulkStats`, `VersionStats`.
- Produces:
  - `#[derive(Serialize)] #[serde(rename_all = "camelCase")] pub struct ContentState { pub phase: Phase, pub views: Option<u64>, pub likes: Option<u64>, pub comments: Option<u64>, pub updated_at: u64, pub show_first_run_note: bool, pub post_coordinate: String, pub post_event_id: String }`.
  - `pub enum Phase { Posting, Live, NotFound, Summary }`, serialized as camelCase strings.
  - `pub enum Decision { Update(ContentState), End(ContentState), Skip }`.
  - `pub fn decide(activity: &StoredActivity, stats: Result<&BulkStats, ()>, views_fallback: Result<Option<u64>, ()>, absent_checks_before: u32, now_unix: u64) -> (Decision, u32)`. The second value is `absent_checks_after`.
  - `pub fn outage_end(activity: &StoredActivity, last_content: Option<&serde_json::Value>, now_unix: u64) -> ContentState`. It returns the last content unchanged, or phase `live` with null counts when nothing was ever sent.
  - `pub fn needs_views_fallback(activity: &StoredActivity, stats: &BulkStats) -> bool`.

- [ ] **Step 1: Write failing tests.** `count_cases` runs every fixture `counts` case through `decide` and asserts `action`, `phase` and the counts. Add `content_state_serializes_exact_keys`, asserting the JSON keys and value types exactly as Global Constraints lists them.
- [ ] **Step 2: Run** and expect FAIL.
- [ ] **Step 3: Implement** per Global Constraints:
  - `Summary` when `now >= window_ends_at`.
  - `NotFound` when past settle and every id is absent from a successful response.
  - Otherwise `Live`, with the settle rule applied per counter after the cross-version maximum.
- [ ] **Step 4: Run** and expect PASS.
- [ ] **Step 5: Commit** `feat(live-activity): decide what each Live Activity update shows`.

### Task 8: Metrics

**Files:** Modify `src/metrics.rs`.

**Interfaces:** Produces counters `live_activity_registrations_total{outcome}`, `live_activity_sends_total{event, result}`, `live_activity_stats_failures_total`, `live_activity_skipped_total{reason}`, and a gauge `live_activity_active`. Follow the existing naming and registration in `src/metrics.rs`.

- [ ] **Step 1:** Add a test asserting the names render at `/metrics` (follow the existing metrics test).
- [ ] **Step 2:** Implement, run, and commit `feat(live-activity): add Live Activity metrics`.

*(Folded into Task 9 if a reviewer judges it too small to stand alone.)*

### Task 9: Worker

**Files:**
- Create: `src/live_activity/worker.rs`
- Modify: `src/main.rs` (spawn with `tracker.spawn("live_activity", None, OnReturn::Tolerated, ...)` beside the existing spawns at `:139-182`, only when `enabled`)
- Test: inline tests with an `AppState` built using the fake Firebase (`FcmClient::new_with_impl`, harness pattern `coalesce.rs:1907-2002`) and an axum Funnelcake stub

**Interfaces:**
- Consumes: Tasks 2, 4, 6, 7, 8.
- Produces:
  - `pub async fn run(state: Arc<AppState>, stats: StatsClient, shutdown: CancellationToken)`.
  - `pub async fn tick(state: &AppState, stats: &StatsClient, now: SystemTime) -> Result<TickReport>`, which is testable.

Each tick runs under `tick_deadline_secs` (it stops starting new work at the deadline, which is under the lease):
- `claim_due(limit 100, lease lease_secs)`.
- One `bulk_grouped` over the claimed activities.
- A `views` fallback where `needs_views_fallback`, run concurrently and capped at `max_concurrency`.
- Each activity is processed under `catch_unwind` (as `send_batch` does, `fcm_sender.rs:603-616`), with sends concurrent and capped at `max_concurrency`. Every end send is preceded by `is_live`.
- `RateLimited` from Funnelcake pushes the whole due set back by `retry_after` (default 60 s) and counts it under `live_activity_stats_failures_total{reason="rate_limited"}`.
- Then for each activity:
  - **`Update`:** send, `record_sent`, then `reschedule(now + cadence)`. If the next run would pass `window_ends_at`, schedule it at `window_ends_at` instead.
  - **`End`:** send, then `finish`.
  - **`Skip`:** `reschedule(now + cadence ± 15 s jitter)`. Once past `window_ends_at` + 600 s with stats still failing, send `End(outage_end(...))` with `dismissal_unix = now - 1` (an immediate removal), then `finish`.
  - **After a send error,** apply `live_activity_send_outcome`: `Finish` → `finish`; `RetryAfter(d)` → reschedule at `now + d`; `RetryNextCycle` → reschedule at `now + cadence`. Never touch `user_tokens:*`.
- `run` sleeps `poll_millis` between ticks that claim nothing, and exits on shutdown. A `tick` error is logged and counted, and the loop continues. `run` returns only on shutdown.

- [ ] **Step 1: Write failing tests:**
  - `tick_sends_update_and_reschedules`
  - `tick_batches_one_bulk_call_for_many_activities`
  - `stats_failure_skips_send_and_reschedules`
  - `window_end_sends_summary_end_and_finishes`
  - `not_found_sends_end_with_dismissal`
  - `token_not_registered_finishes`
  - `end_twice_is_noop`: two ticks over the same claimed job after a lease expiry give exactly one end send.
  - `superseded_activity_is_never_sent`
  - `panic_in_one_activity_is_contained`: a fake Firebase that panics for one token; the others still send and the loop survives.
  - `tick_error_keeps_loop_running`
  - `slow_stats_respects_tick_deadline`
  - `outage_at_window_end_ends_with_last_content_and_immediate_dismissal`
  - `rate_limited_pushes_due_set_back`
  - `token_not_registered_never_prunes_registration`
  - `not_found_needs_two_consecutive_misses`
- [ ] **Step 2: Run** `cargo test live_activity::worker` and expect FAIL.
- [ ] **Step 3: Implement**, and wire the spawn in `main.rs` behind `enabled`.
- [ ] **Step 4: Run** and expect PASS. Then run the full `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --check`.
- [ ] **Step 5: Commit** `feat(live-activity): keep each post's Live Activity current for its first hour`.

### Task 10: Docs

**Files:**
- `docs/nip-xx-push-notifications.md`: in *Registration (kind 3079)* (~`:34`), document `liveActivity` and `notificationsAuthorized`, ending with `state: "ended"`, never 3080, and an example under *Examples*.
- `docs/notification-delivery.md`: name the Live Activity delivery class as outside the caps, gap, quiet hours, app-active refusal and preferences, bounded by the window and cadence.
- `docs/developer-guide.md`: the Live Activity message shape near *iOS APNS shape* (~`:181`), and the new keys in *Redis Keys* (~`:521`).
- `AGENTS.md`: the new keys in the Redis key list, and a row in *Notification Types*.

- [ ] **Step 1:** Write the docs. Every value must match Global Constraints.
- [ ] **Step 2:** Commit `docs(live-activity): document the Live Activity registration, delivery class and keys`.

---

## Part B: divine-mobile (branch `feat/post-live-activity-widget-ui`, PR #11258)

### Task M1: Widget reads Unix-seconds dates

**Files:**
- Modify: `mobile/ios/Shared/PostLiveActivity/PostActivityAttributes.swift` (`PostActivityContentState`)
- Create: `mobile/test/fixtures/live_activity_contract.json`, a byte-identical copy of Part A's file
- Test: `mobile/ios/RunnerTests/PostActivityContentStateCodingTests.swift`

**Interfaces:**
- Produces: `PostActivityContentState` keeps `var updatedAt: Date`, but its `Codable` encodes and decodes the `updatedAt` key as a number of seconds since 1970 (`timeIntervalSince1970`). Every other key stays as it is. Absent `postCoordinate` and `postEventId` still decode as nil.

- [ ] **Step 1: Write failing tests:**
  - `testDecodesEveryFixtureContentState`: read the fixture through `URL(fileURLWithPath: #filePath)` relative to `mobile/test/fixtures/`, decode each `contentStates[].json` with a **default** `JSONDecoder()` (as ActivityKit does), and assert every expected field, `updatedAt` included.
  - `testRoundTripsThroughDefaultCoders`: a default `JSONEncoder` then `JSONDecoder` preserves `updatedAt` to the second.
  - `testFixtureChecksumIsPinned`: the same sha256 as Part A.
  - `testLinksMatchFixture`: `PostActivityLink.url(postEventId:postCoordinate:)` equals each `links[].url` exactly.
- [ ] **Step 2: Run** the RunnerTests (AGENTS.md recipe, `-only-testing:RunnerTests`) and expect FAIL.
- [ ] **Step 3: Implement** a custom `init(from:)` and `encode(to:)`.
- [ ] **Step 4: Run** the full RunnerTests and expect PASS.
- [ ] **Step 5: Commit** `fix(live-activity): read the activity's update time as Unix seconds`.

### Task M2: One home for the timings, and fixture parity

**Files:**
- Modify: `mobile/packages/post_live_activity_repository/lib/src/models/live_activity_registration.dart` (add `static const settle = Duration(minutes: 10);` beside `window`); `mobile/lib/blocs/post_analytics_detail/post_analytics_detail_cubit.dart` (use `LiveActivityRegistration.settle` and `.window` in place of `settleDelay` and `windowLength`, updating callers and tests)
- Test: `mobile/test/live_activity_contract_test.dart`, plus a package test for the new constant

**Interfaces:**
- Produces: `LiveActivityRegistration.settle`. The cubit's public `settleDelay` and `windowLength` are removed, so callers use the package constants.

- [ ] **Step 1: Write failing tests** in `live_activity_contract_test.dart`:
  - `timings match the contract`: `LiveActivityRegistration.settle.inSeconds == timings.settleSeconds` and `.window.inSeconds == timings.windowSeconds`.
  - `checksum is pinned`.
  - `links parse to their post`: every `links[]` url, through the app's deep-link parser, yields that `postEventId` and `postCoordinate`. This closes the Task 9 gap where Swift output was never run through the Dart parser.
  - `single-version count cases`: for each `counts[]` case with one version, `response: "ok"` and `action: "update"`, the cubit's count rule yields the expected views, likes and comments. Drive it through the cubit's existing `load()` with a stubbed repository, as its current tests do.
- [ ] **Step 2: Run** `cd mobile && mise exec -- flutter test test/live_activity_contract_test.dart` and expect FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the affected suites: the cubit, the package with `--coverage` (100% gate) and the contract test. Then `flutter analyze lib test integration_test` and `run_guards.sh` under `mise exec`.
- [ ] **Step 5: Commit** `refactor(live-activity): take the settle and window timings from one place`.

### Task M3: Dim the Lock Screen counts while stale

**Files:**
- Modify: `mobile/ios/Shared/PostLiveActivity/PostActivityViews.swift` (the Lock Screen and expanded count line, `PostActivityDetailLine`, about `:239-260`)
- Test: `mobile/ios/RunnerTests/PostActivityViewsTests.swift`

**Interfaces:**
- Produces: when `PostActivityDisplay.showsUpdatedAt` is true (stale and in progress), the Lock Screen and expanded-island counts render at the same reduced opacity the compact island already uses (0.5, `PostActivityViews.swift:122`). Fresh cards are unchanged. Sebastian reviews the look on PR #11258.

- [ ] **Step 1: Write failing tests:** `testLockScreenCountsDimWhenStale` and `testLockScreenCountsFullWhenFresh`, asserting the opacity through the same inspection approach the existing views tests use.
- [ ] **Step 2: Run** the RunnerTests and expect FAIL.
- [ ] **Step 3: Implement.** Pass `isStale` to the detail line and apply `.opacity`, with a named constant shared with the compact view.
- [ ] **Step 4: Run** the full RunnerTests and expect PASS. Capture before and after screenshots for the PR.
- [ ] **Step 5: Commit** `feat(live-activity): dim the Lock Screen counts while the card is stale`.

---

## Access and rollout checklist (Matt, with Daniel)

- [ ] **Firebase console:** the iOS app's APNs authentication key is present and allows Live Activity pushes (needed for `live_activity_token`). Matt checks whether he has access, and asks Daniel otherwise.
- [ ] **Staging config** (`divine-iac-coreconfig`, `k8s/applications/divine-push-service/overlays/staging/kustomization.yaml`): two `op: add` env entries under the Deployment patch, in the style of `NOSTR_PUSH__NOSTR__RELAY_URL`:
  - `NOSTR_PUSH__LIVE_ACTIVITY__ENABLED="true"`
  - `NOSTR_PUSH__LIVE_ACTIVITY__FUNNELCAKE_BASE_URL="http://funnelcake-api.funnelcake.svc.cluster.local:8080"`, the funnelcake-api ClusterIP service on 8080, which is the same in every environment

  No secret is needed. You also need a Profile or TestFlight build pointed at staging. Deploy with a manual ArgoCD sync (Daniel). During the test, check the funnelcake logs for 429s: behind Linkerd, in-cluster callers may share one rate-limit bucket (unverified).
- [ ] **Staging test send:** post from a real iPhone and watch the activity:
  - update;
  - go stale and dim when the worker is stopped;
  - end with the summary;
  - and, with Funnelcake made unreachable at the hour's end, get removed immediately.

  Capture Firebase's error body for a rejected activity token. This also answers the declined-notifications token question. If the answer is no, the direct-to-Apple fallback becomes a pre-release task.
- [ ] **Kill switch:** turning `live_activity.enabled` off takes a redeploy (an ArgoCD sync). Activities already running go stale and iOS removes them. A runtime pause key was considered and left out of v1.
- [ ] **Production config and deploy**, then the mobile release (#11258 merges only after this).

## Out of scope

- Direct-to-Apple delivery (the named fallback, above).
- Push-to-start (iOS 17.2+).
- Android.
- Changes to Funnelcake.
- A runtime pause key.
- Ending an activity on sign-out: the app ends it on the device, and the server stops at the window end or when Firebase rejects the token. This was accepted in the spec's *When the server is told an activity ended*.
