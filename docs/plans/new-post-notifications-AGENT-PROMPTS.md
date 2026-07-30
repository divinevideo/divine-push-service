# Agent handoff: new-post ("bell") notifications

Companion to [`new-post-notifications.md`](new-post-notifications.md), which is
the design. This file is the **execution packet**: verified cross-repo facts,
then one copy-pasteable prompt per task.

Date: 2026-07-30
Mobile status: **built and tested**, branch `design/bell-notifications` in
`divine-mobile` (6 commits, unpushed at time of writing).
Service status: **not started.**

---

## 1. Why this document exists

The design doc describes the service. This one carries the **client half of
the contract**, verified against mobile code that now exists.

**Read this section before task 4 in the design doc.** Task 4 currently says
mobile *cannot* satisfy the preference dependency, and blocks on it. That is
no longer true.

### Superseded: the client-dependency in design task 4

Design task 4 states, correctly as of 2026-07-29:

> **This is a hard dependency on mobile, and as of 2026-07-29 mobile cannot
> satisfy it.** … `toKindsList()` can only emit a subset of `{1, 3, 7, 16}`,
> so **34236 is never published today** and this check gates every new-post
> push off.

As of 2026-07-30 mobile **can** satisfy it. That paragraph is resolved — do not
treat it as an open blocker.

It works by a **different mechanism** than either version of the design
assumed. The design expected the bell publish and the preference publish to
happen together; they are in fact decoupled:

- A sixth flag `newPostsEnabled` (default `true`) emits `34236`
  (`mobile/lib/models/notification_preferences.dart`).
- Republishing is driven by a **persisted schema-version marker**, not by the
  bell. On session readiness, if the last-published kind-list schema version
  lags the current one, preferences are marked dirty and the existing drain
  publishes them
  (`mobile/lib/services/notification_preferences_service.dart`,
  `publishedKindsSchemaVersion = 1`).

Consequence for this service: **34236 arrives in kind 3083 on the upgraded
client's first ready session**, not on first bell. Still no backfill needed.
Do not write one.

### Still true and load-bearing

- Kind 34236 is already subscribed (`config/settings.yaml:44`).
- `insert_video_reference_fields` (`event_handler.rs:937`) already emits the
  exact routing payload. Do not add new payload fields.
- Design task 4's other correction still holds: the `display_name` string is
  matched in `notificationKindFromPushType`, not `parseFcmPayload`. Mobile now
  knows `newPost`. The tap route resolves to the video even for a value mobile
  does not recognize, so casing drives in-app row typing rather than routing
  (see [2.2](#22-fcm-payload-service-sends-client-reads)).
- The `d=notify` / mobile list-UI collision the design flags as a risk is
  **fixed in mobile** (`Nip51PeopleListCodec.reservedDTags`). The reason it
  mattered to this service — treat an empty `p` list as legitimate, not
  malformed — is unchanged and still required.

---

## 2. Verified wire contract

Every value below was read out of the built mobile code. These are exact.

### 2.1 Subscription list event (client publishes, service reads)

```
kind:       30000
tags:       ["d", "notify"]
            ["title", "Notify"]
            ["p", "<64-char-lowercase-hex>"]   × N
content:    ""
```

- `d` tag is exactly `notify`. Source:
  `Nip51PeopleListCodec.notifyDTag`.
- `title` is exactly `Notify`.
- An **empty `p` set is legitimate** — it is how a user clears every bell. The
  client publishes a `d`/`title`-only event. Do not treat it as malformed.
- Pubkeys are full 64-char hex, never truncated, deduped, and the client never
  writes a self-reference.
- `created_at` is **monotonic per client**: the client steps past the previous
  event's `created_at` when two toggles land in the same second, because
  NIP-33 tie-breaking is unspecified. Your ordering guard should still compare
  `>=`, but you will not see equal timestamps from a well-behaved client.

### 2.2 FCM payload (service sends, client reads)

The `type` string is a **wire contract**, matched exactly by
`notificationKindFromPushType` in
`mobile/lib/notifications/routing/notification_tap_target.dart`.

| key | value | required |
|---|---|---|
| `type` | `newPost` — **camelCase**, not `new_post`, not `newpost` | yes |
| `body` | e.g. `Alice posted a new vine` | **yes — see below** |
| `title` | e.g. `New vine` | no (falls back to `diVine`) |
| `senderPubkey` | creator hex | yes |
| `eventId` | the kind-34236 event id | yes |
| `referencedAddress` | `34236:<creator-hex>:<d-tag>` | yes — this is the tap target |
| `referencedEventId` | the video event id | yes |
| `referencedKind`, `referencedAuthorPubkey`, `referencedDTag` | coordinate components | emitted already |

**`body` must be non-empty or the notification is silently dropped.**
`PushNotificationService.handleForegroundMessage`
(`push_notification_service.dart:299`) returns early and logs
`'Foreground message missing body — skipping local notification'` when `body`
is absent. A payload that is otherwise perfect but has no `body` produces
*nothing* on the device, with no user-visible error. This is the single
easiest way to ship a silently broken feature.

**Casing:** `notificationKindFromPushType('newpost')` and `'NewPost'` both
return `null`. That does **not** break the tap route.
`resolveNotificationTapTarget` routes any non-`follow`, non-`system` kind
with a video target to `OpenVideoTarget`, and `null` is neither
(`notification_tap_target.dart:145-166`), so with `referencedAddress` present
the notification still opens the video — `autoOpenComments` false, which is
what a new post wants. Degrading to a profile/inbox guess happens only when
there is *no* video target, which is not this payload.

What a miscased value costs is the `NotificationKind` mobile maps for in-app
row typing. Match the casing mobile actually ships, and do not rely on step 5
of the end-to-end check to catch a mismatch — the tap succeeds either way.

**Comments must not auto-open.** `notificationKindOpensComments` returns
`false` for `newPost`, which is correct — a new video has no comment to open.
Nothing to do; do not add a `hasCommentTarget` field for this type.

### 2.3 Preference gating

- Gate on kind `34236` present in the user's stored preference kind list.
- `Mention` remains kind `1`, so the two are independently mutable even though
  video mentions also arrive on kind-34236 events. Verified by a mobile test
  (`toKindsList includes kind 34236 independently of mentions`).
- Add `34236` to `UserPreferences::default()` **and** to
  `notification.default_preferences.kinds` in both `config/settings.yaml` and
  `config/settings.development.yaml`.

---

## 3. Task prompts

Each prompt is self-contained. Run them in order — 1→2 and 3→4 have real
dependencies. Prompts 5, 6, 7 are independent of each other once 3 lands.

Every prompt assumes the agent has read `AGENTS.md` and
`docs/plans/new-post-notifications.md` in this repo.

---

### Prompt 1 — subscribe to `d=notify` lists

```
Read docs/plans/new-post-notifications.md (task 1) and
docs/plans/new-post-notifications-AGENT-PROMPTS.md (sections 1 and 2.1) first.

Implement subscription to NIP-51 kind 30000 people lists carrying the exact
`d` tag "notify", in src/nostr_listener.rs.

Requirements:
- Add `const KIND_NOTIFY_LIST: u16 = 30000;` and a `NOTIFY_LIST_D_TAG`
  constant with value "notify".
- Use a SECOND, SEPARATE filter — nostr-sdk 0.44 exposes the NIP-01 `#d`
  filter as `Filter::identifier`. It must not be merged into the existing
  `.kinds(all_kinds)` filter, because an identifier constraint there would
  wrongly apply to every kind in that list. `subscribe` takes one filter per
  call, so issue a second `subscribe`.
- Add the filter to BOTH `process_historical_events` and
  `subscribe_to_live_events`.
- The historical notify-list query must use NO `since` bound. A replaceable
  list published three months ago and untouched since is still current;
  bounding it by `process_window_days` (7) would silently drop most
  subscriptions when the index has to be rebuilt. Add config
  `notify_list_history_limit` (default 5000) as a result-size safety valve
  instead.
- STOP before assuming the previous bullet is enough — it is not, and the
  gap is unresolved in the design. `run()` calls `is_event_too_old(&event)`
  on every event before `route_event` ever sees it (event_handler.rs:93),
  against a hard-coded `REPLAY_HORIZON_DAYS = 7` (event_handler.rs:40). A
  90-day-old d=notify event is dropped there regardless of the filter, so
  removing `since` alone recovers nothing older than a week. Do not invent
  the fix: raise it with the PR author, then implement whatever is decided
  and add a boundary test either side of the horizon. Related, same loop:
  `try_claim_event` (event_handler.rs:105) writes `dedup:{event_id}` with a
  7-day TTL before routing, so an already-processed list event is dropped as
  a duplicate on replay.

Verify: `cargo clippy --all-targets --all-features` and `cargo test` clean.
Do not implement the handler yet — that is task 2. This task should compile
with the events routed nowhere, or routed to a stub that logs and returns Ok.
```

---

### Prompt 2 — ingest lists into the Redis reverse index

```
Read docs/plans/new-post-notifications.md (task 2, including the Redis schema
table) and AGENT-PROMPTS section 2.1 first. Task 1 must be merged.

Implement `handle_notify_list_update` in src/event_handler.rs plus
`replace_notify_subscriptions` in src/redis_store.rs.

CRITICAL — the most likely way to get this silently wrong:
Route kind 30000 in `route_event` BEFORE the control-event block and OUTSIDE
the `is_event_for_service` p-tag gate. These events are addressed to the world,
not to this service. If they go through that gate, every list is dropped and
the feature does nothing with no error anywhere.

Handler requirements:
1. Reject unless the `d` tag is exactly "notify" (defence in depth — the relay
   filter should guarantee it, but a buggy or hostile relay can send anything).
2. Extract `p` values, parse as PublicKey, dedup, drop self-references, then
   truncate to config `notify_list_max_creators` (default 1000) with a warning
   log. The Lua script below runs as one blocking unit and Redis is
   single-threaded and shared with other services, so an unbounded list stalls
   all of them. Truncate rather than reject — the user keeps the bells that fit
   instead of losing every one, and `p` tags are client-ordered so which
   survive is deterministic. Duplicates must not consume cap budget.
   Keep this collection logic (dedup, self-filter, cap) in a PURE SYNC
   function rather than inline in the async handler, or none of it is testable
   without a live Redis and an AppState.
3. Replaceable ordering guard: compare `event.created_at` against
   `notify_subs_ts:{author}`; drop and debug-log when stored >= incoming.
   Without this, a late-delivered older replacement resurrects an unbell.
4. An EMPTY `p` set is legitimate — it is how a user clears every bell. It must
   clear the forward set and remove the subscriber from every reverse index.
   Do NOT treat empty as malformed-and-skip.

Redis: diff-and-apply must be atomic across replicas — use a Lua script keyed
on the subscriber, re-checking the timestamp inside the script (the read in
step 3 is advisory and racy alone). Writing `notify_watchers:*` keys not
declared in KEYS is not Redis-Cluster-safe. That is acceptable, but justify it
from the production topology in the comment, not from docker-compose.yml:
production Redis is master/replica with Sentinel failover — a single keyspace,
not a sharded Cluster — and is shared with other services on a dedicated
logical database. Word the comment so the distinction survives: the manifests
sit under a cluster-flavoured name while not being Redis Cluster, and that is
what a future reader gets wrong in the unsafe direction.

Tests (all required): d-tag rejection; self-reference dropped; add/remove diff
produces correct reverse-index membership; older created_at ignored; empty list
clears everything; the creator list truncates at the cap and duplicates do not
consume cap budget. For the skip-cleanly-without-Redis convention copy
`create_test_pool` from tests/dedup_test.rs, which PINGs before handing back
the pool — NOT `get_test_pool` from tests/preferences_test.rs, which only
checks that create_pool returned Ok and leaves the tests to panic on the bb8
timeout when no server is listening.
```

---

### Prompt 3 — resolve new-post recipients

```
Read docs/plans/new-post-notifications.md (task 3) and AGENT-PROMPTS section
2.2 first. Task 2 must be merged.

Add `NotificationType::NewPost` in src/preferences.rs with:
- `display_name()` returning exactly "newPost" (camelCase — this string is a
  wire contract matched by the mobile client. It does not affect tap routing:
  an unrecognized value still opens the video because `referencedAddress` is
  present. It does decide the `NotificationKind` mobile uses for in-app row
  typing, so it has to match what mobile ships.)
- `kind()` returning 34236

STRUCTURAL CHANGE, unavoidable: `handle_content_event` currently computes a
single `(NotificationType, Vec<PublicKey>)` tuple (event_handler.rs:384). A
kind-34236 event can now produce BOTH Mention recipients (existing "Inspired
by" behaviour) and NewPost recipients. Change the shape to a flat
`Vec<NotificationTarget>` where `NotificationTarget { recipient, notification_type }`,
and iterate targets in the send loop.

`video_notification` becomes async and returns mention targets plus
`SMEMBERS notify_watchers:{event.pubkey}` as NewPost targets.

Dedup rule: MENTION WINS. If a user both watches the creator and is mentioned
in the video, send exactly one push typed Mention. Build mention targets first
and skip any watcher already present. (The mobile client has the same rule for
its in-app rows, so the two stay consistent.)

Filter out watchers equal to `event.pubkey` before the Redis round-trip.

Tests: watcher yields a NewPost target; mentioned watcher yields exactly one
Mention target and NO NewPost target; author watching themselves yields
nothing; no watchers leaves existing mention-only behaviour byte-identical.
```

---

### Prompt 4 — preference gating

```
Read AGENT-PROMPTS section 1 ("Superseded: the client-dependency in design task
4") and section 2.3. NOTE: the design doc's task 4 is stale on how the client
publishes 34236 — AGENT-PROMPTS supersedes it.

Add 34236 to `UserPreferences::default()` and to
`notification.default_preferences.kinds` in BOTH config/settings.yaml and
config/settings.development.yaml.

`NewPost.kind()` returning 34236 means the existing
`notification_type.is_enabled(&prefs)` check at event_handler.rs:624 gates it
with no further change.

DO NOT write a Redis backfill migration for existing `user_preferences:*`
entries. The mobile client republishes kind 3083 including 34236 on the first
ready session after upgrade, driven by a persisted schema-version marker
(NotificationPreferencesService.publishedKindsSchemaVersion). A backfill would
be redundant and would overwrite preferences the client is about to restate.

Tests: `NewPost.kind() == 34236`; `NewPost.is_enabled()` false when prefs omit
34236; enabled under the shipped defaults; Mention still gates on kind 1 so the
two are independent.
```

---

### Prompt 5 — per-creator rate limit

```
Read docs/plans/new-post-notifications.md (task 5). Task 3 must be merged.

Cap delivery at one push per (subscriber, creator) per window. Vines are cheap
to make; an unthrottled prolific creator trains users to disable notifications
entirely.

Add `new_post_rate_limit_secs` to ServiceSettings, default 3600, following the
`default_video_coordinate_dedup_ttl` pattern (config.rs:64).

In `send_notification_to_user`, for NotificationType::NewPost only:
- BEFORE building the payload (alongside the existing video-coordinate check at
  event_handler.rs:634): `get_cached_string` on
  `notify_rate:{recipient}:{creator}`, return early if present.
- AFTER `success_count > 0` (alongside event_handler.rs:732):
  `set_cached_string` with `new_post_rate_limit_secs`.

Use check-then-set-on-success, NOT an atomic `SET NX EX`. Rationale that MUST
go in a code comment so a future reader does not "fix" it: SET NX EX would burn
the user's hour-long window on a failed FCM send. The cost is that two replicas
processing different videos from one creator in the same instant can both pass
the check and double-send — a rare, bounded, low-harm race. Silently eating an
hour of notifications on an FCM blip is worse.

The limit is PUSH-ONLY. The in-app feed shows every post; a user who gets one
push for a six-post burst opens the app and sees all six. That asymmetry is
intended, not a bug.

Verify explicitly with a test rather than assuming: the existing per-recipient
coordinate claim (`video_recipient_claim_key`, event_handler.rs:533) is keyed
on {kind}:{author}:{d_tag}:{recipient} and should already prevent a NIP-33
EDIT of the same video from re-notifying. Confirm that holds for NewPost
targets.

Tests: second video from same creator inside window suppressed; video from a
DIFFERENT creator in the same window delivered; window marker NOT written when
every FCM send fails; an edited video does not re-notify.
```

---

### Prompt 6 — FCM copy

```
Read AGENT-PROMPTS section 2.2. Task 3 must be merged.

Add the NotificationType::NewPost arm to `create_fcm_payload`
(event_handler.rs:804):

    NotificationType::NewPost => (
        "New vine".to_string(),
        format!("{} posted a new vine", sender_name),
    ),

`body` MUST be non-empty. The mobile client silently drops any notification
without a `body` (push_notification_service.dart:299 — early return, warning
log, no user-visible error). Assert this in a test.

`sender_name` resolution via `mention_parser_service` already works for
kind-34236 events; no change needed.

The copy above is PROVISIONAL and matches what mobile currently renders for its
in-app row. Both need sign-off against
divine-mobile/brand-guidelines/TONE_OF_VOICE.md before release. Flag it in the
PR description rather than treating it as approved.
```

---

### Prompt 7 — docs

```
Update, in this repo:
- README.md "Notification types" table: add the New post row (kind 34236,
  triggered by a watched creator posting). The table currently ends at Repost.
- docs/nip-xx-push-notifications.md: specify the d=notify kind-30000 list —
  tag shape, replaceable semantics, empty-list-means-cleared, and that it is
  PUBLIC (anyone can read who a user subscribes to; this was a deliberate
  portability tradeoff, not an oversight).
- docs/developer-guide.md: the four Redis key families, reverse-index
  maintenance, and the rate-limit window. Include the FCM payload contract
  from AGENT-PROMPTS section 2.2, since that is where clients will look.
- AGENTS.md: add the new keys to "Redis Keys" and NewPost to "Notification
  Types". Both tables go stale the moment task 2 lands.
```

---

## 4. End-to-end verification

Cannot be done from this repo alone — needs the mobile branch on a device.

1. Bell creator B from account A in the app. Confirm a kind 30000 event with
   `["d","notify"]` and B's full 64-char `p` tag reaches the relay.
2. Confirm `notify_watchers:{B}` contains A.
3. Confirm A's stored preferences in Redis contain 34236. **If they do not, the
   client's schema-marker republish did not run — do not work around it here,
   it is a mobile bug.**
4. Post a video as B. Confirm A receives a push titled "New vine".
5. **Tap it.** Confirm it opens the video, not A's profile or the inbox. This
   will pass even if `type` is miscased, so it does not verify the casing —
   check the in-app row renders as a new-post row for that.
6. Post again within the hour. Confirm no second push and a rate-limit skip in
   the logs.
7. Unbell B. Confirm the republished list drops B's `p` tag and pushes stop.
8. Unfollow B while belled. Confirm the bell and subscription both clear — the
   client publishes the teardown from a host above the bell, so this works even
   though the bell unmounts.

## 5. Known gaps outside this repo

- **divine-funnelcake has no owner for the in-app row.** The push is
  rate-limited; the in-app feed is not, so FunnelCake needs its own fan-out
  rather than mirroring what this service sent. It would be the inbox's first
  subscription-driven fan-out — every existing source resolves recipients from
  tags on the event itself, whereas new-post rows have no `p` tag and need a
  join against the `d=notify` lists. Mobile already accepts a `newPost` row
  type; it renders nothing until FunnelCake emits one.
- **Mobile ships a live bell and a live settings toggle for a feature with no
  backend.** Until this service deploys, both are inert. Whether that merges
  behind a feature flag is an open product decision.
