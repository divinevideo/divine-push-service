# Like and Repost notification coalescing

Status: Revision 11 — implementation plan. Supersedes revision 10.

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
- Revision 9: applies the four corrections from the revision-8 review and
  refreshes citations against current `main`. Architecture is unchanged.
  1. Reconciliation never re-adds a group with nothing pending.
  2. Reconciliation must never requeue a group a live flush owns. The
     implementation enforces that by making only expired lease entries
     candidates and checking `due` before re-adding (see Reconciliation).
  3. The dependency on #72 is named: flush-time preference revalidation reads
     stored preferences, and #72 says an update can be dropped for a week.
  4. The logical-expiry grace setting is named and validated
     (`coalesce_logical_expiry_grace_secs`); mixed-version rollout is listed as
     an accepted loss.
- Revision 10 (this document): the full revision-8 text and its independent
  review became available during review of the revision-9 implementation, and
  three corrections are folded back in:
  1. **Two lifetimes, restored.** A group has an absolute logical lifetime
     (`coalesce_group_ttl_secs`, 24 h) and a longer physical TTL for cleanup
     (logical + `coalesce_logical_expiry_grace_secs`). Queued work no longer
     dies at the window-plus-grace TTL; the flush and retry paths delete and
     count a group at logical expiry while the hash still names it. Revision 9
     used a three-hour physical TTL and a silent dangling-member drop, which
     could lose a summary with no attribution under a same-boundary backlog.
  2. **The throttle bounds summaries too.** A summary flush spends one
     recipient token before sending; an empty bucket defers the group to the
     refill time instead of sending, and a group still throttled at logical
     expiry is dropped loudly and counted. This is revision 8's disposition
     table, and it is what actually bounds recipient volume after a shared
     bucket boundary.
  3. **Bounded drain.** One flush is capped at
     `coalesce_flush_timeout_secs` (120 s), so a stalled profile lookup or FCM
     batch cannot pin the drain behind one group; the lease outlives twice that
     bound and expiry recovers a timed-out flush. Flush outcomes (expired,
     dangling, empty, suppressed, deferred) are logged and counted.
- Revision 11 (this document): the product owner decided both open items on
  2026-09-15, and this revision records the decisions and their implementation.
  1. **The shared collapse id is confirmed.** One collapse id for the immediate
     pushes and the summary is the intended behavior; each immediate replacing
     the previous banner and FCM possibly discarding a pending individual push
     in favour of a later one are accepted consequences of the single-banner
     behavior. Distinct immediate ids (four banners where there were three plus
     a summary) were considered and rejected. Per-post thread grouping is a
     possible future mobile change, explicitly out of scope here.
  2. **A rolling 24-hour emission cap bounds the recipient budget.** A recipient
     may emit at most `recipient_daily_cap` like/repost notifications
     (immediates and summaries both count) in the rolling
     `recipient_daily_window_secs` window. At the cap the work defers; a
     deferred summary that is still capped at its logical expiry is dropped
     loudly and counted, exactly as the throttle disposition table already
     said. As part of the decision the burst capacity was lowered from 60 to 20
     (`recipient_throttle_capacity`) so the burst and the rolling daily budget
     agree. The refill stays one token per minute, and the 24-hour window is
     what bounds a permanently over-budget recipient; the ~1,440-push/day
     ceiling revision 10 documented is gone.

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
- Grouping follows the **directly acted-upon** reference: the lowercase `a`
  coordinate, or the **last** lowercase `e` tag (NIP-25 puts the reacted event
  last when a reaction copies root and reply tags). An uppercase NIP-22 `A`/`E`
  root is only a fallback, so a reaction that carries its target's root does not
  merge with reactions on other objects under the same root. The summary payload
  keeps the root-aware reference fields the immediate payload uses, so routing
  is unchanged.

The per-recipient token bucket demotes a would-be immediate push into the
bucket instead of sending it. The immediate slot is not consumed by a
throttled event. A summary flush spends one token before sending, so the
bucket also bounds the summaries that can leave at a shared bucket boundary: an
empty bucket defers the group to the next refill instead of sending, and a
still-throttled group at its logical expiry is dropped loudly and counted.

On top of the bucket, a rolling emission window bounds how many like/repost
notifications one recipient can receive: at most `recipient_daily_cap` emitted
notifications per `recipient_daily_window_secs`, counting immediates and
summaries. The check is the same decision point as the bucket spend, on both
paths; a recipient at the cap is demoted or deferred even when the bucket has
tokens, and the oldest emission aging out of the window admits the next one.

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
| `coalesce:g:{type}:{owner}:{target}:{bucket}` | Hash | Bucket group state: `due`, `expires_at`, `pending`, `immediate`, `first_actor`, `first_event`, `last_at`, `event_kind`, `owner`, `type`, `target`, reference fields, and the current `lease` token. Physical TTL `coalesce_group_ttl_secs + coalesce_logical_expiry_grace_secs` once it has buffered work; window + grace while it has only immediate traffic |
| `coalesce:hll:{type}:{owner}:{target}:{bucket}` | HyperLogLog | Distinct buffered actors. Same physical TTL |
| `coalesce:disp:{event_id}:{recipient}` | String | Per-event disposition, `i:{gid}` or `b:{gid}`, so a replay reproduces the original decision and collapse id. TTL `coalesce_window_secs + coalesce_logical_expiry_grace_secs` |
| `coalesce:due` | Sorted Set | Groups due for flush, scored by bucket deadline |
| `coalesce:leases` | Sorted Set | Groups currently owned by a flush, scored by lease expiry |
| `coalesce:throttle:{owner}` | Hash | Per-recipient token bucket: `tokens`, `ts` |
| `coalesce:emitted:{owner}` | Sorted Set | Per-recipient rolling emission window: members are the emitted notification identities (trigger event id for an immediate push, group id for a summary) scored by the Redis server time of the send. `ZREMRANGEBYSCORE` drops members older than `recipient_daily_window_secs` on every read and `ZCARD` is the count checked against `recipient_daily_cap`. TTL `recipient_daily_window_secs` + grace; a re-spend of the same member does not inflate the count |

`target` is `e:{event-id-hex}` for an event reference or `a:{kind:pubkey:d-tag}`
for an addressable coordinate. Reference fields (`ref_event_id`, `ref_address`,
`ref_kind`, `ref_author`, `ref_dtag`) are copied from the trigger events so the
summary payload can reproduce the same routing fields the immediate payload
carries.

### Two lifetimes

A group carries `expires_at = creation + coalesce_group_ttl_secs` (24 h), and
its physical TTL is that lifetime plus the cleanup grace. The separation exists
because a bare Redis TTL cannot produce a loud, counted drop: expiry deletes
the hash, taking the recipient, type, and target with it, and the reconciler
then sees only an index member whose key is gone. It cannot tell a deliberate
loss from a backlog, a retry, or corruption.

A group that has only immediate traffic is never claimed and is not durable
work, so it keeps the shorter window-plus-grace TTL: it only needs to outlive
its bucket for a later buffered event in the same bucket. The long lifetime
applies once a group has buffered work, and an immediate event on such a group
does not shorten it.

So the ownership-checked flush and retry paths check `expires_at` and delete
and count a group that crossed it while the evidence is still readable. The
longer physical TTL only reclaims genuinely abandoned state. The counter and
warning name the recipient, type, and target, and `expires_at` is compared
against the Redis server clock, the clock that wrote it.

A backlog, a repeatedly failing send, or a permanently throttled recipient can
all reach the logical lifetime; that drop is an accepted loss below, and the
difference from revision 9 is that it is attributable rather than silent. The
drain is serial per replica and each flush is timeout-bounded; the oldest-due
gauge is the signal that a backlog is not clearing (the alerting rule that
watches it is the operator follow-up tracked with the Redis sizing in
`divinevideo/divine-iac-coreconfig#1932`), and the lifetime is what bounds how
long any item waits.

### Token bucket and rolling emission cap

Capacity `recipient_throttle_capacity`, refill one token per
`recipient_throttle_refill_secs`. State is stored with millisecond timestamps;
the bucket refills lazily on read and never exceeds capacity. The key expires
after `max(2 * capacity * refill, window + grace)`.

The bucket charges only for emitted notifications. The immediate path spends a
token at ingest; the flush path spends one before sending. A summary that
reached no device and failed retryably returns its token, so a stuck group
cannot drain each refill and starve the recipient's immediate pushes. A
deferral spends nothing.

The rolling emission cap is the outer bound. Each emitted notification is a
member of `coalesce:emitted:{owner}` scored with the Redis server time of the
send; before any spend both paths remove members older than
`recipient_daily_window_secs` and refuse when `ZCARD` has reached
`recipient_daily_cap`. At the shipped defaults a recipient can emit a 20-push
burst (the bucket capacity, deliberately the same as the daily cap so the burst
cannot exceed it), the bucket then refills one token a minute, and the rolling
window still holds the recipient to 20 emitted like/repost notifications in any
24 hours. That replaces the ~1,440-push/day ceiling revision 10 documented: a
recipient permanently over budget now defers until the oldest emission ages
out, and a summary deferred past `coalesce_group_ttl_secs` is dropped loudly
and counted. A retryable all-token failure refunds both the token and the
window slot, because nothing was emitted.

A refusal reports which bound refused it and the delay that clears it: an empty
bucket requeues the group at the token refill, while a full window requeues it
at the moment the oldest member ages out (`oldest + window - now`). Without
that distinction a capped group would re-poll the serial drain every refill for
its whole lifetime, and the deferral counter would measure retries instead of
distinct deferrals. A member is identified per emission — the trigger event id
for an immediate push, the group id for a summary — so a re-spend after a
recovered lease cannot charge the window twice.

## Ingest

One atomic Lua script (`coalesce:ingest`), invoked only after the allowlist,
token, and preference gates pass and after the per-`(event, recipient)` claim is
held. It:

1. returns the stored disposition when one exists (replay);
2. otherwise reads the server clock, computes the bucket and deadline, and
   checks the immediate budget, the recipient token bucket, and the rolling
   emission window;
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
2. drop the group and count it if it has reached `expires_at` (see Two
   lifetimes); the drop names the recipient, type, and target;
3. revalidate the allowlist, the recipient's registered tokens, and the stored
   notification preference. A group dropped by any of these is completed
   without a push and counted under its reason;
4. spend one recipient token and record the emission in the rolling window. An
   empty bucket requeues the group at the refill time, and a full window
   requeues it when the oldest emission ages out; either way the deferral is
   counted and is not a failure. A deferral that would cross `expires_at` drops
   the group instead;
5. resolve the first buffered actor's display name, build the summary payload
   with the group's collapse key, and send it to all of the recipient's tokens;
6. refresh token activity from delivered tokens and prune invalid tokens, as
   the immediate path does;
7. complete the group under the ownership check. A success (at least one
   delivered token) deletes the group and HLL. When no token was delivered the
   token and the window slot spent in step 4 are refunded, whether the failure
   was retryable or terminal, because nothing was emitted. A retryable failure
   then requeues the group after FCM's `Retry-After` when it is larger than
   `coalesce_retry_secs` (`coalesce_retry_secs` is a floor, not a ceiling; a
   delay that would cross `expires_at` drops the group instead). A terminal
   failure (every token non-retryable) deletes the group, counts
   `push_coalesce_skipped_total{reason="send_failed"}`, and warns with the
   recipient, type, and target.

The whole flush is bounded by `coalesce_flush_timeout_secs` (120 s). A timed-out
flush leaves its lease in place; lease expiry recovers the work, and a summary
that already left is a duplicate rather than a loss. Without the bound, one
stalled name lookup or FCM batch would pace the entire drain.

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
- **Logical expiry, counted before the re-add.** A pending group whose
  `expires_at` has passed is deleted and counted by this step instead of being
  requeued, while its hash still names the recipient, type, and target.
- A dangling index member whose group key is already gone is removed, logged,
  and counted. With the two lifetimes this is a defensive path for eviction,
  legacy state, or a physically expired key, not the ordinary outcome of a
  backlog.

## FCM collapse

- Android stays data-only; the collapse key goes in the `android` block as
  `collapse_key`.
- iOS gets `apns-collapse-id` in the `headers` block.
- The key is derived from the group id: the first 16 bytes of its BLAKE3 digest,
  hex-encoded (32 characters, within APNs' 64-byte limit). The immediate pushes
  and the summary for the same group share the key.
- **What collapse does per platform.** `apns-collapse-id` replaces the
  displayed banner in Notification Center. Because the immediate pushes and the
  summary share one id, on iOS each immediate push replaces the previous banner
  and the summary replaces the last: a user watching the burst sees only the
  newest banner, and FCM may discard a pending individual push in favour of the
  later one. That is intended — it is what makes the burst a single banner —
  and it is the product-confirmed behavior (2026-09-15): one evolving banner
  per post, with the replacement and possible discard accepted as the cost.
  Distinct immediate ids, which would preserve each immediate banner and send
  the summary as a separate one, were considered and rejected. Per-post thread
  grouping is a possible future mobile change and is out of scope here. FCM's
  `android.collapse_key` coalesces messages *queued while the device is
  offline*; it does not replace a notification the app has already posted.
  Android data-only pushes are rendered by the app, and the current client
  derives its local notification id from the message timestamp rather than the
  collapse key, so on Android a burst still shows the immediate banners plus the
  summary. Replacing them requires a client change to key its local notification
  on the collapse key; that is out of scope here and is listed as an accepted
  loss with the on-device verification gap.

## Configuration

New `service` settings, each rejected at zero:

| Setting | Default | Meaning |
|---------|---------|---------|
| `coalesce_window_secs` | 7200 | Bucket length |
| `coalesce_immediate_limit` | 3 | Immediate pushes per bucket per group |
| `coalesce_lease_secs` | 300 | Flush lease; must exceed twice the flush timeout |
| `coalesce_retry_secs` | 5 | Requeue delay after an all-token retryable failure |
| `coalesce_poll_millis` | 250 | Idle poll interval |
| `coalesce_group_ttl_secs` | 86400 | Absolute logical lifetime; must exceed window + lease |
| `coalesce_flush_timeout_secs` | 120 | Hard bound on one flush |
| `coalesce_logical_expiry_grace_secs` | 3600 | Physical-TTL grace past the logical lifetime; also the disposition-TTL grace |
| `recipient_throttle_capacity` | 20 | Per-recipient burst capacity; matches the daily cap so the burst cannot exceed it |
| `recipient_throttle_refill_secs` | 60 | Seconds per refilled token |
| `recipient_daily_window_secs` | 86400 | Rolling window for the per-recipient emission cap |
| `recipient_daily_cap` | 20 | Emitted like/repost notifications per recipient per window (immediates and summaries) |

## Metrics

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `push_coalesced_sends_total` | Counter | `type` (`like`/`repost`) | Summary pushes sent |
| `push_coalesce_oldest_due_age_seconds` | Gauge | — | Age of the oldest group that is due now, measured against the Redis server clock and sampled on every worker pass |
| `push_throttled_recipients_total` | Counter | `type` | Would-be immediate pushes demoted by the per-recipient like/repost budget (empty token bucket or rolling emission cap reached) |
| `push_coalesce_skipped_total` | Counter | `reason` | Terminal non-delivery outcomes: `expired`, `expired_throttled`, `send_failed`, `dangling_due`, `missing_group`, `no_pending`, `unknown_type`, `missing_actor_count`, `unparseable_owner`, `not_allowlisted`, `no_tokens`, `preference_disabled` |
| `push_coalesce_deferred_total` | Counter | `reason` | Flushes deferred without consuming an attempt (`recipient_throttled`: token bucket empty; `recipient_daily_capped`: rolling emission cap reached) |
| `push_coalesce_flush_failures_total` | Counter | `reason` | Flushes that failed before a terminal outcome (`timeout`, `error`) |

## Accepted losses

- **A collapse key can replace an undelivered earlier push.** The immediate
  pushes and the summary share one id, so on iOS each immediate push replaces
  the previous banner, the summary replaces the last, and FCM may discard a
  pending individual push in favour of a later one. Product-confirmed on
  2026-09-15: this is how a burst becomes one evolving banner per post, and the
  replacement and possible discard are accepted. Distinct immediate ids with
  the summary as a separate banner were rejected; per-post thread grouping is
  out of scope.
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
- **A group that reaches its logical lifetime is lost, loudly and counted.**
  Any group not completed within `coalesce_group_ttl_secs` (24 h) is deleted by
  the flush, retry, or reconciliation path with a warning naming the recipient,
  type, and target, and a `push_coalesce_skipped_total{reason="expired"}`
  increment. That covers sweeper death, a backlog past the lifetime, repeated
  retries, and a permanently throttled recipient. The long physical TTL then
  only reclaims abandoned state.
- **A recipient at the emission cap loses the summary at logical expiry.** The
  bucket and the rolling window defer rather than drop, but deferral is bounded
  by `coalesce_group_ttl_secs`; a recipient who stays at `recipient_daily_cap`
  for the group's whole 24-hour lifetime reaches it. At the shipped defaults the
  window and the lifetime are both 24 h, so this is the expected outcome for a
  recipient who is continuously at the cap, not an edge case. A group whose own
  burst filled the window loses its summary at the first flush instead of after
  the lifetime: its own immediate emission is the oldest member, so the slide
  lands exactly at the group's `expires_at` and `complete_group` drops the group
  rather than requeueing a retry that would already be past it. Loud and counted
  under `push_coalesce_skipped_total{reason="expired_throttled"}`, which keeps
  the `expired` counter for backlog losses; the deferral itself is not a drop.
- **Redis footprint grows per interaction.** Every immediate like/repost also
  writes a group hash, a disposition string, and a member of the recipient's
  rolling emission window (`coalesce:emitted:{owner}`, bounded to
  `recipient_daily_cap` entries per active recipient); every buffered one adds a
  HyperLogLog and a `coalesce:due` member. An immediate-only group hash keeps
  the window-plus-grace TTL, but a group with buffered work and its HLL live
  for the logical lifetime plus grace (25 h by default) instead of the
  window-plus-grace three hours, on top of today's `dedup:` claim. The
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

- config: every new setting rejected at zero; the relational bounds (group
  lifetime > window + lease, lease > twice the flush timeout) enforced; shipped
  configs carry each new key with its documented value.
- ingest: first N immediate, the rest buffered; replay returns the original
  decision; bucket rollover resets the immediate budget and gives replay a
  stable collapse id; grouping follows the directly acted-upon reference (the
  last lowercase `e`, per NIP-25) rather than the root scope.
- throttle: a recipient at capacity has an immediate push demoted into the
  bucket, and the demoted event does not consume an immediate slot; a summary
  flush with an empty bucket is deferred to the refill time without sending,
  keeps its buffered work, and sends once the bucket refills; an all-token
  retryable failure refunds the spent token and requeues at FCM's
  `Retry-After` when it exceeds the configured floor.
- daily cap: with `recipient_daily_cap` emissions in the rolling window, an
  immediate push is demoted and a summary flush is deferred without sending or
  spending a token, and the deferred summary keeps its buffered work; the
  requeue lands when the oldest emission ages out, not at the token refill, and
  that due score is asserted; once the window slides the next immediate sends
  and the deferred summary sends; a re-spend of the same emission (the
  recovered-lease duplicate path) does not inflate the window count; an
  all-token failure that delivered nothing refunds the token and the window
  slot whether it was retryable or terminal, and the terminal case is counted
  under `send_failed`.
- durability: a buffered group carries `expires_at` and a physical TTL longer
  than the window plus grace; an immediate-only group keeps the short bucket
  TTL; a flush drops and deletes a group past its logical lifetime; a requeue
  that would cross the lifetime drops instead of requeueing.
- claim/lease: an expired lease is reclaimed; an empty group is deleted rather
  than re-added; completion requires the lease token; a dangling index member
  is dropped and counted.
- metrics: the oldest-due-age sample reads the head of the due queue against
  server time; expired, empty, suppressed, and dangling outcomes are counted.
- delivery: a flush sends one summary; a panicking FCM send is contained and
  the group still completes; the collapse key reaches the Android and APNs
  transport blocks on the wire.

Open: on-device collapse behavior (above).
