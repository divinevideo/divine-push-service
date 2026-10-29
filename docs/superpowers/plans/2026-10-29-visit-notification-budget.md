# Visit Notification Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Batch social activity and reset the account interruption allowance on an authenticated foreground visit without bypassing cooldown or duplicate protection.

**Architecture:** Extend push-service's Redis policy and leased coalescer. An authenticated Funnelcake gateway forwards lifecycle sessions from mobile to push-service. Independent repository workers implement the fixed activity contract in parallel.

**Tech Stack:** Rust, Redis Lua, Axum, Funnelcake NIP-98 API, Flutter/Dart.

**Spec:** ../specs/2026-10-29-visit-notification-budget.md

## Global Constraints

- At most two nonurgent pushes per absence; minimum six-hour gap across visits.
- Presence expiry 180 seconds; heartbeat 60 seconds.
- Quiet hours 21:00–06:59; unknown timezone suppresses nonessential delivery.
- Strict authenticated activity contract; 4096-byte body; sequence and visit 1..2147483647; visit <= sequence.
- No unread acknowledgements or provider-token exposure; no production deployment.

## Review Focus

- A delayed background or foreground retry cannot refill an already opened session.
- An open between lease and final send must invalidate old queued activity.
- One device going background must not clear another device's foreground lease.
- An ambiguous provider attempt must not acquire a fresh budget after a visit.
- Delayed signing/account switches must never report activity as another account.

### Task 1: Push activity authority and interruption policy

**Files:** Create src/activity.rs and tests/activity_test.rs; modify src/server.rs,
src/lib.rs, src/notification_delivery.rs, src/config.rs, config/settings.yaml,
tests/notification_delivery_test.rs.

**Interfaces:** record_activity(pool, recipient, ActivityUpdate) applies strict
session ordering; visit generation and presence use Redis server time.
reserve_budget remains the shared final gate and retains historical identities.
Expose generation-aware authorization for the coalescer, including retry delay.

- [x] Write and run failing tests for reset, heartbeat, ordered background,
  stale/duplicate activity, multiple devices, expired presence, concurrent budget,
  six-hour cooldown, replayed identity, and old generation.
- [x] Implement Redis activity state and authenticated bounded internal endpoint.
- [x] Integrate atomic visit/cooldown/presence checks with final delivery policy.
- [x] Run Redis-backed tests, cargo fmt, cargo clippy, and cargo test.
- [x] Commit the task after its relevant checks pass.

### Task 2: One account summary from the first interaction

**Files:** src/coalesce.rs; related focused tests; docs/plans/like-repost-coalescing.md.

**Interfaces:** Consume activity generation/final authorization from Task 1.
Produce a single immutable recipient/bucket summary across target/type, with
separate like/repost counts and notification-inbox tap routing.

- [x] Write failing tests for three immediate likes becoming one buffered digest,
  multiple posts/types sharing a summary, replay deduplication, preferences, and
  open-during-lease invalidation.
- [x] Extend existing leased groups instead of introducing another worker.
- [x] Recheck final policy and defer cooldown-bound work without busy polling.
- [x] Run focused Redis-backed tests and commit the verified task.

### Task 3: Account-authenticated activity gateway

**Repository:** divine-funnelcake, isolated feat/notification-activity-gateway branch.
**Files:** new crates/api/src/notification_activity.rs; router, lib, startup config,
and OpenAPI registrations following repository patterns.
**Interfaces:** Public and internal activity contract from the spec; internal
recipientPubkey derives exclusively from NIP-98 signer.

- [x] Run baseline and write failing auth, input, forwarding, and failure tests.
- [x] Implement bounded strict request parsing, signature/body binding, and
  two-second no-redirect forwarding with redacted credentials.
- [x] Run focused tests, formatting, and required API checks; commit.

### Task 4: Mobile visit lifecycle and digest routing

**Repository:** divine-mobile, isolated feat/notification-visit-activity branch.
**Files:** funnelcake_api_client activity method; focused lifecycle coordinator and
provider; startup side effects; existing push tap handling and targeted tests.
**Interfaces:** Public activity contract from the spec. Process/account UUID; monotonic
sequence and explicit visit counter across visits in one process; foreground heartbeat 60 seconds; no heartbeat after background/dispose.

- [ ] Run baseline and write failing lifecycle, account-switch, retry/signing,
  background-fetch, and combined-summary routing tests.
- [ ] Implement owner-bound authenticated activity; preserve transient inactive
  state without generating visits; route account digests to Notifications.
- [ ] Run focused Flutter/package tests, formatting, and analyze; commit.

### Task 5: Review and delivery

- [ ] Review all three diffs in fresh independent context and fix grounded issues.
- [ ] Run cross-contract consistency checks and relevant required validation.
- [ ] Push each isolated branch, open linked PRs, request mapped review teams once,
  and inspect required/relevant CI results. Preserve merge/deployment authority.
- [ ] Report implementation, evidence, rollout dependencies, and remaining limits.

## Validation record

Push service: `CARGO_INCREMENTAL=0 REDIS_URL=redis://127.0.0.1:6388 cargo test --all-targets`
passed 352 tests. `CARGO_INCREMENTAL=0 cargo clippy --all-targets --all-features -- -D warnings`,
`cargo fmt --all -- --check`, and `git diff --check` passed.

Gateway: 17 focused integration tests, focused Clippy with warnings denied,
startup compilation, formatting, and diff checks passed. The broad baseline
suite exposed an existing profile-search failure and a long-running archive
provenance test; all-target Clippy reports 14 existing errors in unchanged
legacy tests. See the gateway checkpoint for exact commands and logs.

Mobile: initial checks passed; independent review found interactive signer
prompts and first-visit recovery after offline startup. Fixes and final review
are in progress. The mobile PR must remain draft until real base/head simulator
screenshots demonstrate lifecycle and digest tap behavior with safe test data.
