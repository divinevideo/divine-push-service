# Like and Repost notification coalescing

Status: Revision 9 — implementation plan. Supersedes revision 8.

Scope: kinds 7 (Like) and 16 (Repost) only. Comments (1111), mentions
(30023/34236), and new-post ("bell") notifications are not coalesced; they
either have no reliably retrievable durable inbox row or are already one push
per event by design.

Tracking: [divine-push-service#75](https://github.com/divinevideo/divine-push-service/issues/75).
Related dependency: [#72](https://github.com/divinevideo/divine-push-service/issues/72)
stored preferences can be dropped for a week, which bounds the flush-time
preference promise below.

## Revision history

- Revision 8: reviewed design. A first-3-immediate / two-hour-bucket coalescing
  scheme with FCM collapse, a per-recipient token bucket, and a leased outbox.
- Revision 9 (this document): applies the four corrections from the revision-8
  review and refreshes citations against current `main`. Architecture is
  unchanged.
  1. Reconciliation never re-adds a group with nothing pending.
  2. Reconciliation requires absence from **both** the `due` and `leases`
     indexes before re-adding a group.
  3. The dependency on #72 is named: flush-time preference revalidation reads
     stored preferences, and #72 says an update can be dropped for a week.
  4. The logical-expiry grace setting is named and validated
     (`coalesce_logical_expiry_grace_secs`); mixed-version rollout is listed as
     an accepted loss.

## Problem

Without aggregation, one popular post buzzes its author once per like. A video
with 500 likes produces 500 pushes. The service currently bounds this with a
production allowlist; removing the allowlist without coalescing exposes the
full registered user base to one-push-per-event behavior.

## Behavior and copy rules

For each `(recipient, notification type, target)` pair, time is divided into
fixed `coalesce_window_secs` buckets aligned to the Redis server clock.

- **First `coalesce_immediate_limit` interactions in a bucket send
  immediately**, with today's copy ("New like" / "`{name}` liked your post").
- **The rest collect in the bucket** and flush once at the bucket boundary as a
  summary:
  - one buffered actor — title `New like`, body "`{name}` liked your post";
  - two buffered actors — title `New likes`, body
    "`{name}` and 1 other liked your post";
  - more than two — title `New likes`, body
    "`{name}` and `{total - 1} others liked your post"`.
  - Repost summary uses `New reposts` / "`{name}` and 1 other
    reposted your post" / "`{name}` and `{total - 1} others reposted your
    post" / "`{name}` reposted your post".
- The name in the summary is the first buffered actor, resolved at flush time;
  the count is the exact distinct-actor count of the buffered set.
- Like and Repost groups are separate: a post with two likes and ten reposts
  produces two summaries, never one mixed-count summary.
- A target that carries neither an `e`/`E` event reference nor an `a`/`A`
  addressable coordinate cannot be summarized ("others liked your post" would
  cross posts), so those events keep today's immediate path and are not
  coalesced.
- Grouping follows the **directly acted-upon** reference: the lowercase `e`/`a`
  tag. An uppercase NIP-22 `A` root is only a fallback, so a reaction that
  carries its target's root coordinate does not merge with reactions on other
  objects under the same root. The summary payload keeps the root-aware
  reference fields the immediate payload uses, so routing is unchanged.

The per-recipient token bucket demotes a would-be immediate push into the
bucket instead of sending it. The immediate slot is not consumed by a
throttled event.

### Why fixed buckets

Bucket identifiers and deadlines are derived from the Redis server clock
(`TIME`) inside the ingest script, so every replica agrees and no client clock
participates. A fixed bucket makes a group immutable once its deadline passes:
an event that arrives after the deadline computes the next bucket and can never
join a group that is being flushed. That property is what makes completion safe
without re-reading the buffered set.

## Redis schema

| Key | Type | Purpose |
|-----|------|---------|
| `coalesce:g:{type}:{owner}:{target}:{bucket}` | Hash | Bucket group state: `due`, `pending`, `immediate`, `first_actor`, `first_event`, `last_at`, `event_kind`, `owner`, `type`, `target`, reference fields, and the current `lease` token. TTL `coalesce_window_secs + coalesce_logical_expiry_grace_secs` |
| `coalesce:hll:{type}:{owner}:{target}:{bucket}` | HyperLogLog | Distinct buffered actors. Same TTL |
| `coalesce:disp:{event_id}:{recipient}` | String | Per-event disposition, `i:{gid}` or `b:{gid}`, so a replay reproduces the original decision and collapse id. TTL `coalesce_window_secs + coalesce_logical_expiry_grace_secs` |
| `coalesce:due` | Sorted Set | Groups due for flush, scored by bucket deadline |
| `coalesce:leases` | Sorted Set | Groups currently owned by a flush, scored by lease expiry |
| `coalesce:throttle:{owner}` | Hash | Per-recipient token bucket: `tokens`, `ts` |

`target` is `e:{event-id-hex}` for an event reference or `a:{kind:pubkey:d-tag}`
for an addressable coordinate. Reference fields (`ref_event_id`, `ref_address`,
`ref_kind`, `ref_author`, `ref_dtag`) are copied from the trigger events so the
summary payload can reproduce the same routing fields the immediate payload
carries.

### Token bucket

Capacity `recipient_throttle_capacity`, refill one token per
`recipient_throttle_refill_secs`. State is stored with millisecond timestamps;
the bucket refills lazily on read and never exceeds capacity. The key expires
after `max(2 * capacity * refill, window + grace)`.

## Ingest

One atomic Lua script (`coalesce:ingest`), invoked only after the allowlist,
token, and preference gates pass and after the per-`(event, recipient)` claim is
held. It:

1. returns the stored disposition when one exists (replay);
2. otherwise reads the server clock, computes the bucket and deadline, and
   checks the immediate budget and the recipient token bucket;
3. for an immediate decision, increments `immediate` and records the
   disposition;
4. otherwise increments `pending`, adds the actor to the HLL, records the
   first buffered actor/event and the latest event timestamp, and adds the
   group to `coalesce:due` (`ZADD NX`);
5. sets the disposition with the group id.

Because the whole decision is one script, two replicas cannot double-count an
event even without the claim; the claim is kept because it already serializes
`(event, recipient)` and because buffered events must not be re-buffered after
a replay.

The immediate send proceeds down today's path, with one addition: the FCM
payload carries a collapse key derived from the group id. On a retryable FCM
failure the recipient claim is released as today, and the replay re-reads the
disposition and resends with the same collapse key.

## Flush

A leased outbox worker runs in both replicas
(`coalesce_flush`, critical for health). Each iteration claims at most one due
group atomically:

1. reconcile expired leases (see below), bounded per call;
2. take the oldest group with `score <= now` from `coalesce:due`, remove it
   there, add it to `coalesce:leases` with the lease deadline, and write a
   random lease token into the group hash.

Processing a claimed group:

1. read the group snapshot (`HGETALL` plus `PFCOUNT`); the group is immutable
   after its deadline, so the two reads cannot disagree about buffered work;
2. revalidate the allowlist, the recipient's registered tokens, and the stored
   notification preference. A group dropped by any of these is completed
   without a push;
3. resolve the first buffered actor's display name, build the summary payload
   with the group's collapse key, and send it to all of the recipient's tokens;
4. refresh token activity from delivered tokens and prune invalid tokens, as
   the immediate path does;
5. complete the group under the ownership check. A success (at least one
   delivered token) deletes the group and HLL. An all-token retryable failure
   requeues the group after `coalesce_retry_secs`. A non-retryable failure is
   logged and the group is completed.

The ownership token is what makes a reclaimed lease safe: only the worker that
still owns the lease may delete or requeue the group. A worker whose lease
expired mid-flush completes nothing, which can duplicate a summary but cannot
delete work another worker owns. That duplicate is an accepted loss below.

Preference revalidation reads stored preferences. #72 tracks that a preference
update can be dropped for a week; during that window a user who disabled
likes can still receive a buffered summary. That is the same guarantee the
immediate path has today, not a regression, and it is why the flush does not
promise more.

## Reconciliation

The claim script reconciles expired lease entries before claiming new work, in
one atomic step:

- **Nothing pending is not work.** An expired entry whose group has
  `pending == 0` (all of its events were sent immediately, or its buffered set
  was already flushed) is deleted, never re-added. This covers the case the
  revision-8 review flagged.
- **No duplicate against a competing live entry.** Only expired entries are
  iterated, so a live flush is never a reconciliation candidate. After the
  expired member is removed, a pending group is re-added to `coalesce:due` only
  when it is absent from `coalesce:due`; the `leases` predicate in the script is
  a defensive assertion that reads false at that point by construction.
- A dangling index member whose group key expired is removed and skipped.

## FCM collapse

- Android stays data-only; the collapse key goes in the `android` block as
  `collapse_key`.
- iOS gets `apns-collapse-id` in the `headers` block.
- The key is derived from the group id: the first 16 bytes of its BLAKE3 digest,
  hex-encoded (32 characters, within APNs' 64-byte limit). Immediate and
  buffered pushes for the same group share the key.
- **What collapse does per platform.** `apns-collapse-id` replaces the
  displayed banner in Notification Center, so on iOS the summary replaces the
  earlier immediate banners. FCM's `android.collapse_key` coalesces messages
  *queued while the device is offline*; it does not replace a notification the
  app has already posted. Android data-only pushes are rendered by the app, and
  the current client derives its local notification id from the message
  timestamp rather than the collapse key, so on Android a burst still shows the
  immediate banners plus the summary. Replacing them requires a client change
  to key its local notification on the collapse key; that is out of scope here
  and is listed as an accepted loss with the on-device verification gap.

## Configuration

New `service` settings, each rejected at zero:

| Setting | Default | Meaning |
|---------|---------|---------|
| `coalesce_window_secs` | 7200 | Bucket length |
| `coalesce_immediate_limit` | 3 | Immediate pushes per bucket per group |
| `coalesce_lease_secs` | 300 | Flush lease |
| `coalesce_retry_secs` | 5 | Requeue delay after an all-token retryable failure |
| `coalesce_poll_millis` | 250 | Idle poll interval |
| `coalesce_logical_expiry_grace_secs` | 3600 | Grace added to the bucket window for group/disposition TTLs |
| `recipient_throttle_capacity` | 60 | Per-recipient burst capacity |
| `recipient_throttle_refill_secs` | 60 | Seconds per refilled token |

## Metrics

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `push_coalesced_sends_total` | Counter | `type` (`like`/`repost`) | Summary pushes sent |
| `push_coalesce_oldest_due_age_seconds` | Gauge | — | Age of the oldest group that is due now, sampled on every worker pass |
| `push_throttled_recipients_total` | Counter | `type` | Would-be immediate pushes demoted by the token bucket |

## Accepted losses

- **FCM's four-collapse-key-per-device limit.** A device tracking more than
  four groups at once can have older groups' collapse behavior degraded. The
  summary still delivers; only the replacement behavior is affected.
- **Android banners are not replaced by the summary today.** FCM's
  `android.collapse_key` coalesces messages queued while the device is offline;
  it does not replace a notification the app already posted, and the current
  client derives its local notification id from the timestamp. A burst on
  Android therefore shows the immediate banners plus the summary. Replacing
  them needs a client change to key the local notification on the collapse key.
  The coalescing value — four banners instead of five hundred — stands either
  way, and the key is already on the wire so the client change is not blocked
  on a service deploy.
- **Duplicate summaries after a crash.** A worker that sends and dies before
  completing leaves the group to be reclaimed after its lease expires, so the
  summary can be sent twice. On iOS the collapse id makes the duplicate replace
  the first banner; on Android it is an extra banner until the client change
  above. The individual likes remain in the inbox either way.
- **Mixed-version rollout during the deploy that lands this change.** A replica
  still running the old build sends like/repost pushes immediately with no
  group or collapse key, and can process an event the new build already buffered
  (or vice versa) before the per-recipient claim is visible across replicas.
  The window is one rolling deploy, the result is extra immediate pushes that
  the new build would have coalesced, and a summary may double-count an actor
  who was also sent immediately. Both are visible as extra banners, not lost
  notifications.
- **Redis footprint grows per immediate interaction.** Every immediate
  like/repost also writes a group hash and a disposition string; every buffered
  one adds a HyperLogLog and a `coalesce:due` member, all on the window-plus-
  grace TTL (three hours by default) on top of today's `dedup:` claim. The
  coordinate half of the target is attacker-chosen, exactly as the existing
  `dedup:{kind}:{type}:{owner}:{d-tag}:{recipient}` key already is, but the
  number of such keys is higher. The capacity sizing that gates allowlist
  removal (`divinevideo/divine-iac-coreconfig#1932`) must account for this
  schema.
- **No client-side verification here.** On-device Android and iOS checks that
  the data-only contract is intact and that collapse behaves as documented are
  required by the issue but cannot run in this environment. They are recorded
  as an open verification gap.

## Verification

Committed regression tests:

- config: every new setting rejected at zero; shipped configs carry each new
  key with its documented value.
- ingest: first N immediate, the rest buffered; replay returns the original
  decision; bucket rollover resets the immediate budget and gives replay a
  stable collapse id; grouping follows the direct reference rather than the
  NIP-22 root.
- throttle: a recipient at capacity has an immediate push demoted into the
  bucket, and the demoted event does not consume an immediate slot.
- claim/lease: an expired lease is reclaimed; an empty group is deleted rather
  than re-added; completion requires the lease token; a dangling index member
  is dropped.
- metrics: the oldest-due-age sample reads the head of the due queue.
- delivery: a flush sends one summary; a panicking FCM send is contained and
  the group still completes; the collapse key reaches the Android and APNs
  transport blocks on the wire.

Open: on-device collapse behavior (above).
