# Plan: new-post notifications ("bell" subscriptions)

Status: ready for implementation
Date: 2026-07-29
Companion repos: `divine-mobile` (bell UI + list publishing), `divine-funnelcake` (in-app feed row)

## Problem

Every notification this service emits today is *"someone acted on your
content"* — a like, comment, repost, or mention, all resolved from `p`
tags on the trigger event. Users have asked for the inverse: **alert me
when a creator I care about posts**, which is a subscription to someone
else's output rather than a reaction to mine.

Nothing in the current model supports per-creator targeting. Preferences
are a flat list of event kinds (`preferences.rs:14`); the service has no
notion of which creators a given user cares about.

## Approach

The subscription list lives on Nostr as a NIP-51 people list with a
reserved `d` tag, published by the mobile client:

```
kind 30000
["d", "notify"]
["title", "Notify"]
["p", "<creator-pubkey-hex>"]   × N
```

This mirrors the existing `d=block` reserved-list precedent already used
by `divine-mobile`'s `content_blocklist_repository`. It is public and
portable — any Nostr client can read and honor it.

This service subscribes to those lists, maintains a reverse index in
Redis (`creator → [subscribers]`), and on each incoming kind 34236 video
notifies every subscriber of that video's author, rate-limited to one
push per (subscriber, creator) per hour.

### What already exists and needs no work

Two things make this smaller than it looks. Verify both before starting:

- **Kind 34236 is already subscribed** (`config/settings.yaml:44`,
  landed in #29 for "Inspired by" mentions). Video events already reach
  `handle_content_event` (`event_handler.rs:420`).
- **The FCM routing payload already exists.**
  `insert_video_reference_fields` (`event_handler.rs:937`) emits
  `referencedAddress` = `34236:<creator>:<d-tag>`, `referencedEventId`,
  `referencedKind`, `referencedAuthorPubkey`, and `referencedDTag` for
  any kind-34236 trigger. The mobile tap router already prefers
  `referencedAddress`. **Do not add new payload fields.**

### What the client is responsible for

Out of scope here, listed so the contract is unambiguous:

- Publishing and maintaining the `d=notify` list (add on bell, remove on
  unbell, remove on unfollow — the bell is follow-gated).
- Including kind `34236` in its kind-3083 preferences event. See
  [Preference gating](#4-preference-gating) for why this matters and why
  no server-side backfill is needed.

## Redis schema

Three new key families. All hex pubkeys, lowercase, full length — never
truncate Nostr identifiers.

| Key | Type | Purpose |
|---|---|---|
| `notify_subs:{subscriber_hex}` | SET | Creators this user has belled. Needed to diff against an incoming replacement list. |
| `notify_subs_ts:{subscriber_hex}` | STRING | `created_at` of the last applied list event. Guards against out-of-order relay delivery of a replaceable event. |
| `notify_watchers:{creator_hex}` | SET | Subscribers watching this creator. The hot read path — one `SMEMBERS` per incoming video. |
| `notify_rate:{subscriber_hex}:{creator_hex}` | STRING w/ TTL | Rate-limit window marker. |

`notify_subs` and `notify_watchers` are two views of the same relation;
they must be updated together (see task 2).

---

## Tasks

Each task is independently verifiable. Run `cargo check` after each,
`cargo clippy --all-targets --all-features` and `cargo test` before
moving on. Follow the repo's TDD expectation: write the failing test
first where a test is called for.

### 1. Subscribe to `d=notify` people lists

**Files:** `src/nostr_listener.rs`, `src/config.rs`, `config/settings.yaml`

Add `const KIND_NOTIFY_LIST: u16 = 30000;`.

Subscribing to *all* kind 30000 would pull every people list on the
relay. Narrow it with the NIP-01 `#d` filter — `nostr-sdk` 0.44 exposes
this as `Filter::identifier`:

```rust
Filter::new()
    .kind(Kind::from(KIND_NOTIFY_LIST))
    .identifier(NOTIFY_LIST_D_TAG)   // "notify"
    .since(since_timestamp)
```

This is a **second, separate filter** — it cannot be merged into the
existing `.kinds(all_kinds)` filter, because an identifier constraint
there would wrongly apply to every kind in the list. Both
`process_historical_events` and `subscribe_to_live_events` need the
extra filter; `subscribe` accepts one filter per call, so issue a second
`subscribe` call.

**Historical processing is mandatory here.** `route_event` skips content
events when `context == EventContext::Historical`
(`event_handler.rs:192`), but notify lists must be rebuilt from history
or a restart silently drops every subscription until each user happens
to republish. Treat them like control events: process in both paths.

Make the historical lookback for notify lists independent of
`process_window_days` (7 days). A list published 3 months ago and never
touched since is still current — it is a replaceable event. Use no
`since` bound at all for the historical notify-list query, and add
config `notify_list_history_limit` (default `5000`) as a safety valve on
result size.

### 2. Ingest list events into the reverse index

**Files:** `src/event_handler.rs`, `src/redis_store.rs`

Route kind 30000 in `route_event` **before** the control-event block,
and critically **outside** the `is_event_for_service` p-tag gate — these
events are addressed to the world, not to this service. Getting this
wrong means every list is silently dropped.

```rust
if kind_num == KIND_NOTIFY_LIST {
    return handle_notify_list_update(state, event).await;
}
```

`handle_notify_list_update`:

1. Reject unless the `d` tag is exactly `"notify"` (defense in depth —
   the relay filter should already guarantee this, but a malicious or
   buggy relay can send anything).
2. Extract `p` tag values, parse as `PublicKey`, dedup, drop
   self-references (belling yourself is meaningless).
3. **Replaceable ordering guard:** compare `event.created_at` against
   `notify_subs_ts:{author}`. If the stored value is `>=` the incoming
   one, drop the event and log at debug. Relays can deliver an older
   replacement after a newer one; without this, an unbell can be
   resurrected.
4. Diff against `notify_subs:{author}` and apply.

Add `redis_store::replace_notify_subscriptions(pool, subscriber, creators, created_at)`.
The diff-and-apply must be atomic across replicas — do it in a **Lua
script** keyed on the subscriber:

```
-- KEYS[1] = notify_subs:{sub}, KEYS[2] = notify_subs_ts:{sub}
-- ARGV[1] = created_at, ARGV[2..] = creator hexes
-- Re-check the timestamp inside the script; the read in step 3 is
-- advisory and racy on its own.
```

The script reads the old set, computes added/removed, `SREM`s the
subscriber from `notify_watchers:{removed}`, `SADD`s to
`notify_watchers:{added}`, replaces `notify_subs`, and sets
`notify_subs_ts`. Writing to `notify_watchers:*` keys not declared in
`KEYS` is not cluster-safe; this deployment uses single-instance Redis
(`docker-compose.yml`), so that is acceptable — **add a comment saying
so** and gate on it if the deployment ever moves to Redis Cluster.

An empty `p` tag list is legitimate (user unbelled everyone) and must
clear the forward set and remove them from every reverse index. Do not
treat empty as "malformed, skip".

**Tests:** `d`-tag rejection; self-reference dropped; add/remove diff
produces the right reverse-index membership; an older `created_at` is
ignored; empty list clears everything.

### 3. Resolve new-post recipients

**Files:** `src/event_handler.rs`, `src/preferences.rs`

Add `NotificationType::NewPost` to the enum (`preferences.rs:50`),
with `display_name() == "newPost"` and `kind() == 34236`.

The `display_name` string is a wire contract — `divine-mobile` matches
on it in `parseFcmPayload` (`mobile/lib/services/notification_helpers.dart`).
Do not change it casually.

**Structural change required.** `handle_content_event` currently
computes a single `(NotificationType, Vec<PublicKey>)` tuple
(`event_handler.rs:384`). A video can now produce *both* Mention
recipients (existing behavior) and NewPost recipients. Change the shape
to a flat list of targets:

```rust
struct NotificationTarget {
    recipient: PublicKey,
    notification_type: NotificationType,
}
```

Each `find_*` branch returns `Vec<NotificationTarget>`; the send loop
iterates targets instead of `(type, recipients)`.

`video_notification` becomes async and returns mention targets plus
`SMEMBERS notify_watchers:{event.pubkey}` as NewPost targets.

**Dedup rule: Mention wins.** If a user both watches the creator and is
mentioned in the video, send one push, typed `Mention` — the more
specific signal. Implement by building mention targets first and
skipping any watcher already present.

Also skip watchers equal to `event.pubkey` — the existing
`recipient_pubkey == event.pubkey` check in the send loop covers this,
but filtering earlier avoids a pointless Redis round-trip per video.

**Tests:** watcher yields a NewPost target; mentioned watcher yields
exactly one Mention target and no NewPost target; author watching
themselves yields nothing; no watchers yields the existing
mention-only behavior unchanged.

### 4. Preference gating

**Files:** `src/preferences.rs`, `config/settings.yaml`

`NewPost.kind()` returns `34236`, so the existing
`notification_type.is_enabled(&prefs)` check at `event_handler.rs:624`
gates it with no changes. Note `Mention` returns `1` even for video
mentions, so the two are cleanly separable — a user can mute new-post
pushes without muting video mentions.

Add `34236` to `UserPreferences::default()` and to
`notification.default_preferences.kinds` in both
`config/settings.yaml` and `config/settings.development.yaml`.

**No server-side backfill is needed, and none should be written.**
Users with a stale `user_preferences:*` entry lacking 34236 would be
silently gated off — but reaching this code path at all requires having
belled someone, which requires the new mobile build, which publishes
kind 3083 including 34236 at the same moment it publishes the notify
list. The client-side ordering makes the migration unnecessary.

**This is a hard dependency on the mobile side.** Confirm the mobile PR
publishes preferences alongside the first bell before shipping this
service change, or new-post pushes will be gated off for every existing
user and the bug will look like a service failure.

**Tests:** `NewPost.kind() == 34236`; `NewPost.is_enabled()` false when
prefs omit 34236; enabled under the shipped defaults.

### 5. Per-creator rate limit

**Files:** `src/redis_store.rs`, `src/event_handler.rs`, `src/config.rs`, `config/settings.yaml`

Vines are cheap to make; an unthrottled prolific creator trains users to
disable notifications entirely. Cap delivery at one push per
(subscriber, creator) per window.

Add config `new_post_rate_limit_secs` to `ServiceSettings`, default
`3600`, following the `default_video_coordinate_dedup_ttl` pattern
(`config.rs:64`).

In `send_notification_to_user`, for `NotificationType::NewPost` only:

- **Before** building the payload (alongside the existing video
  coordinate check at `event_handler.rs:634`), `get_cached_string` on
  `notify_rate:{recipient}:{creator}` and return early if present.
- **After** `success_count > 0` (alongside `event_handler.rs:732`),
  `set_cached_string` with `new_post_rate_limit_secs`.

Use check-then-set-on-success rather than an atomic `SET NX EX`, to
mirror the existing video-coordinate dedup and — more importantly — so
a failed FCM send does not burn the user's hour-long window. The
tradeoff is that two replicas processing different videos from the same
creator within the same instant can both pass the check and double-send.
That is a rare, bounded, low-harm race; silently eating an hour of
notifications on an FCM blip is worse. **Write this rationale into a
code comment** so a future reader does not "fix" it into `SET NX EX`.

The rate limit is **push-only**. The in-app feed (FunnelCake) shows
every post from belled creators. A user who receives one push for a
six-post burst opens the app and sees all six — that is intended.

**Interaction with existing video dedup, verify explicitly:** the
per-recipient coordinate claim (`video_recipient_claim_key`,
`event_handler.rs:533`) is keyed on
`{kind}:{author}:{d_tag}:{recipient}` and already prevents a NIP-33
*edit* of the same video from re-notifying. That protection applies to
NewPost targets for free. Confirm with a test rather than assuming it.

**Tests:** second video from the same creator inside the window is
suppressed; a video from a *different* creator in the same window is
delivered; the window marker is not written when every FCM send fails;
an edited video does not re-notify.

### 6. FCM copy

**File:** `src/event_handler.rs` (`create_fcm_payload`, line 804)

Add the `NotificationType::NewPost` match arm:

```rust
NotificationType::NewPost => (
    "New vine".to_string(),
    format!("{} posted a new vine", sender_name),
),
```

`sender_name` resolution via `mention_parser_service` already works for
kind-34236 events — no change needed.

Copy is **provisional**. `divine-mobile/brand-guidelines/TONE_OF_VOICE.md`
governs user-facing strings; get this string confirmed before release
rather than treating the placeholder above as approved.

### 7. Docs

**Files:** `README.md`, `docs/nip-xx-push-notifications.md`, `docs/developer-guide.md`, `AGENTS.md`

- README "Notification types" table: add the New post row (kind 34236,
  triggered by a watched creator posting). The table currently ends at
  Repost.
- Protocol doc: specify the `d=notify` kind-30000 list — tag shape,
  replaceable semantics, and that it is public.
- Developer guide: document the four Redis key families, the reverse
  index maintenance, and the rate-limit window.
- `AGENTS.md`: add the new keys to the "Redis Keys" list and NewPost to
  the "Notification Types" table. Both are stale the moment task 2
  lands.

---

## Verification

```bash
cargo clippy --all-targets --all-features
cargo test
```

Redis-backed integration tests follow the existing `tests/dedup_test.rs`
convention (skip cleanly when Redis is unavailable). Add
`tests/notify_subscriptions_test.rs` covering the index round-trip:
publish list → assert reverse index → publish shrunken list → assert
removal → assert the ordering guard rejects a replayed older event.

Manual end-to-end, once the mobile branch is available:

1. Bell creator B from account A; confirm the kind 30000 `d=notify`
   event lands on the relay with B's `p` tag.
2. Confirm `notify_watchers:{B}` contains A.
3. Post a video as B; confirm A receives a push titled "New vine" and
   that tapping it opens the video.
4. Post a second video as B within the hour; confirm no second push and
   that the log records the rate-limit skip.
5. Unbell B; confirm `notify_watchers:{B}` no longer contains A and no
   further pushes arrive.

## Risks

**Relay volume.** The `#d=notify` filter keeps list ingestion cheap, but
the service already receives every kind-34236 event on the relay. This
change adds one `SMEMBERS` per video. Should that become hot, cache
`notify_watchers:{creator}` in-process with a short TTL — do not
pre-optimize.

**Public subscription lists.** The `d=notify` list is world-readable, so
who you have belled is public. This was an accepted product decision in
favor of portability, not an oversight. Do not "fix" it by encrypting
the content field — that would make the list unreadable to this service
and break the feature entirely.

**Client dependency.** Task 4 is gated on the mobile client publishing
34236 in its preferences. Ship order matters: service first is safe (it
simply finds no watchers), client-first is safe (pushes start when the
service deploys). Only a *partial* client rollout that publishes notify
lists without updating preferences produces silent failure.
