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

Four new key families. All hex pubkeys, lowercase, full length — never
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
or the reverse index cannot be recovered after Redis data loss — every
subscription stays dark until each user happens to republish. (A process
restart on its own is fine — Redis is external, so the index survives
it.) Treat them like control events: process in both paths.

Make the historical lookback for notify lists independent of
`process_window_days` (7 days). A list published 3 months ago and never
touched since is still current — it is a replaceable event. Use no
`since` bound at all for the historical notify-list query, and add
config `notify_list_history_limit` (default `5000`) as a safety valve on
result size.

**Dropping `since` is not sufficient, and this is still unresolved.**
`run()` calls `is_event_too_old(&event)` on *every* event before it
reaches `route_event` (`event_handler.rs:93`), against a hard-coded
`REPLAY_HORIZON_DAYS = 7` (`event_handler.rs:40`). A 90-day-old
`d=notify` event is dropped in the handler loop whatever the filter says
and whatever order `route_event` uses, so as written above the rebuild
recovers nothing older than a week — exactly the case it exists to
protect.

The mechanism is the author's call: a kind-aware horizon, an explicit
exemption for list events, or something else. Whichever way it goes,
task 1 needs a boundary test either side of the horizon. Note also that
`try_claim_event` (`event_handler.rs:105`) runs before `route_event` and
writes `dedup:{event_id}` with a 7-day TTL, so replaying a list event
that was already processed is dropped as a duplicate — the plan should
say how the rebuild is meant to interact with that.

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
script** keyed on the subscriber.

Why atomic, concretely: production runs **2 replicas**
(`divine-iac-coreconfig`, `k8s/applications/divine-push-service/overlays/production/kustomization.yaml`;
staging and poc run 1, no HPA). `try_claim_event` only prevents two
replicas handling the *same* event — two different list events from one
subscriber can still land concurrently, and a read-then-write lets the
older one win, resurrecting an unbell. `MULTI`/`EXEC` cannot express
this: it queues blind writes, so the old set would have to be read
outside the transaction. `WATCH` + retry would also work but is more
code over pooled connections. A single-replica deployment needs none of
this — the handler loop is sequential within a process.

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
`KEYS` is not Redis-Cluster-safe. That is acceptable here, but for a
production reason rather than the local `docker-compose.yml`: the
deployment points at
`redis://redis-replication-master.redis-clusters.svc.cluster.local:6379/2`
(`divine-iac-coreconfig`,
`k8s/applications/divine-push-service/base/deployment.yaml:44`), and
`k8s/redis-clusters/base/cluster.yaml` declares a `RedisReplication`
plus a `RedisSentinel` — master/replica with Sentinel failover, one
keyspace, no sharding. Cross-key writes are fine. **Add a comment saying
so**, and say it in those terms: the namespace is called
`redis-clusters` but is not Redis Cluster, which is exactly the sort of
thing a future reader gets wrong in the unsafe direction. Gate on it if
the deployment ever moves to real Cluster mode.

That Redis is also **shared** — this service uses db index 2 of an
instance other services key off, and Sentinel is watching its liveness.
Anything that blocks it blocks them.

**Bound the creator list before it reaches Redis.** The script runs as
one blocking unit and Redis is single-threaded, so a list with thousands
of `p` tags stalls the instance — for every user of this service and,
because the instance is shared, for every other service on it, with
Sentinel watching. Add config
`notify_list_max_creators` (default `1000`, well above any follow-gated
bell list) and truncate with a warning rather than rejecting the list —
the user keeps the bells that fit instead of losing all of them, and `p`
tags are client-ordered so which survive is deterministic. This matters
more than the cluster-safety note above: Cluster is hypothetical, an
oversized list is something a user can do today.

An empty `p` tag list is legitimate (user unbelled everyone) and must
clear the forward set and remove them from every reverse index. Do not
treat empty as "malformed, skip".

**Tests:** `d`-tag rejection; self-reference dropped; add/remove diff
produces the right reverse-index membership; an older `created_at` is
ignored; empty list clears everything; the creator list truncates at the
cap and duplicates do not consume cap budget.

Keep the collection logic (dedup, self-filter, cap) in a **pure sync
function** rather than inline in the async handler, or none of it is
testable without a live Redis and an `AppState`.

### 3. Resolve new-post recipients

**Files:** `src/event_handler.rs`, `src/preferences.rs`

Add `NotificationType::NewPost` to the enum (`preferences.rs:50`),
with `display_name() == "newPost"` and `kind() == 34236`.

The `display_name` string is a wire contract — `divine-mobile` matches on
it in `notificationKindFromPushType`
(`mobile/lib/notifications/routing/notification_tap_target.dart:90`),
which recognizes exactly five lowercase values
(`like`/`comment`/`follow`/`mention`/`repost`) and returns `null` for
anything else.

An unrecognized value is **non-fatal, and the tap still routes
correctly.** `resolveNotificationTapTarget` sends any non-`follow`,
non-`system` kind with a video target to `OpenVideoTarget`, and `null` is
neither (`notification_tap_target.dart:145-166`). Because this service
always emits `referencedAddress` for a kind-34236 trigger,
`hasVideoTarget` is true, so the notification opens the video with
`autoOpenComments` false — correct for a new post. No client change is
needed for routing to work.

What the string does drive is the `NotificationKind` mobile uses for
in-app row typing, so still do not change it casually. Note every value
mobile recognizes today is lowercase, which argues for `newpost` over
`newPost`; that is a cross-repo contract decision, not a detail to settle
by whichever repo lands first.

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
belled someone, which requires the new mobile build. Provided that build
publishes kind 3083 including 34236 alongside the notify list, the
client-side ordering makes a migration unnecessary.

**This is a hard dependency on mobile, and as of 2026-07-29 mobile
cannot satisfy it.** `NotificationPreferences`
(`mobile/lib/models/notification_preferences.dart`) is five fixed
booleans, and `toKindsList()` can only emit a subset of `{1, 3, 7, 16}`.
`push_notification_service.dart:254` publishes exactly that list, so
**34236 is never published today** and this check gates every new-post
push off.

That is not a rollout-timing risk to be confirmed before shipping — it
is the current state, and it fails silently. Mobile needs a sixth
preference flag mapping to 34236 before any bell can produce a push.
Tracked in the mobile design doc
(`divine-mobile/docs/plans/2026-07-29-bell-notifications-design.md`,
finding 2). Shipping this service change first is still safe: it finds
no watchers and does nothing.

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
  lands. The existing "Redis Keys" entries are also **wrong today**:
  they document `divine:token:{pubkey}`, `divine:preferences:{pubkey}`
  and `divine:dedup:{pubkey}:{event_id}`, and no `divine:`-prefixed key
  exists anywhere in `src/`. The real prefixes are `user_tokens:` and
  `dedup:` (`redis_store.rs:21-27`), `user_preferences:`
  (`preferences.rs:88`), plus the fixed `stale_tokens` and
  `token_to_pubkey`. Correct them in the same pass instead of appending
  four accurate rows to a list of three inaccurate ones.

---

## Verification

```bash
cargo clippy --all-targets --all-features
cargo test
```

Redis-backed integration tests follow the existing `tests/dedup_test.rs`
convention (skip cleanly when Redis is unavailable). Copy **that**
helper: `create_test_pool` (`tests/dedup_test.rs:6`) issues a `PING` and
returns `None` when it fails. Do not copy `get_test_pool`
(`tests/preferences_test.rs:10`), which only checks that `create_pool`
returned `Ok` — it does with no server listening, and the tests then
panic on the bb8 timeout instead of skipping. Add
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
34236 in its preferences, which it does not do today (see task 4). Ship
order between the repos is still safe in both directions: service first
finds no watchers and does nothing, client-first starts delivering when
the service deploys. The unsafe case is a *partial* client rollout that
publishes notify lists without the preference change — pushes are gated
off silently and it looks like a service failure. That is the current
mobile state, so the mobile release must carry both.

**Reserved `d` tag collides with the mobile list UI.** `d=notify` decodes
as an ordinary user-editable people list in `divine-mobile`
(`Nip51PeopleListCodec` excludes only `d=block`), so before the mobile
fix lands a user can rename or delete the list and silently wipe their
own subscriptions. Nothing the service can defend against — it just sees
a replacement list — but it is why the service must treat an empty `p`
list as legitimate (task 2) rather than assuming malformed input.
