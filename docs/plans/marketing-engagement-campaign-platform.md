# Plan: marketing and engagement campaign platform

Status: archived discovery brief; implementation moved to `divine-engagement`
Date: 2026-07-30
Owners: product, marketing/engagement, creator team, engineering

> This document records the product and service-boundary discovery that led to
> the dedicated `divine-engagement` repository. The implementation repository's
> architecture and API documentation are authoritative for current behavior.

## Summary

Build an internal campaign platform that helps Divine bring people back to
creators, conversations, and useful product actions without turning
notifications into spam.

The platform should be a new dedicated repository, tentatively named
`divine-engagement`, served at `engagement.admin.divine.video` and linked from
the existing `divine-admin-dashboard`.

This is not a replacement for `divine-push-service`:

- `divine-engagement` owns campaigns, segments, approvals, scheduling,
  experiments, audit history, and delivery orchestration.
- `divine-push-service` remains the only service that owns FCM credentials,
  device-token resolution, notification delivery policy, and FCM delivery.
- Divine analytics/FunnelCake owns behavioral facts used to resolve segments.
- `divine-mobile` owns device metadata, notification preferences, tap routing,
  and client-side outcome measurement.

The first release supports staff test sends and explicit pubkey lists. It does
not include a "send to everyone" action, automatic copy generation, arbitrary
SQL segments, or creator-authored broadcasts.

## Why a new repository

Divine already has `divine-admin-dashboard`, but it is intentionally a small
Cloudflare Access-protected directory of links to dedicated admin tools. It
does not provide a shared application shell, database, job runner, or role
model.

A campaign platform has a distinct lifecycle and risk profile:

- campaigns live for days or weeks;
- recipients are resolved and scheduled asynchronously;
- quiet hours create recipient-local deferred work;
- delivery must survive retries and deployment;
- every action needs an audit trail;
- production sends require stronger authorization than ordinary navigation;
- campaign and recipient history should not share the real-time push service's
  Redis lifecycle.

Keeping this in its own repository makes those concerns explicit while
preserving `divine-push-service` as a focused delivery service.

## Product principles

1. Every interruption must earn its place.
2. Every message says why the recipient received it.
3. Every tap opens exactly what the message promised.
4. Marketing and recommendations are independently controllable from social
   notifications.
5. No marketing or engagement delivery from 21:00 through 06:59 recipient-local
   time.
6. Quiet hours are enforced by the delivery system, not left to campaign
   authors.
7. If consent, timezone, audience provenance, or destination is uncertain, do
   not send.
8. Optimize for meaningful actions and retained trust, not raw sends or opens.
9. No false urgency, fabricated social activity, guilt, streak anxiety, or
   vague "something is waiting" copy.
10. Every material campaign has a holdout group.

## Users and roles

### Campaign author

Usually a member of marketing, engagement, creator support, or product.

Can:

- create and edit drafts;
- choose an approved segment;
- upload an explicit pubkey list;
- preview copy and tap behavior;
- send to internal test recipients;
- request approval.

Cannot:

- approve their own production campaign;
- bypass consent, frequency caps, quiet hours, or campaign expiry;
- create arbitrary executable audience queries.

### Campaign approver

Can:

- inspect audience logic, exclusions, previews, schedule, expiry, holdout, and
  projected delivery;
- approve or reject a campaign;
- pause or cancel delivery;
- record the reason for a decision.

### Campaign operator

Can:

- inspect live progress and failures;
- retry approved retryable failures;
- pause, resume, or cancel delivery;
- use the emergency global stop.

Cannot change approved copy or audience without creating a new campaign
revision and approval.

### Auditor/read-only

Can inspect campaign definitions, approvals, delivery summaries, experiments,
and immutable audit history without creating or changing anything.

## Initial use cases

### Test delivery

Send a real notification to one or more staff pubkeys to verify:

- title and body rendering;
- Android and iOS behavior;
- tap destination;
- timezone and quiet-hours behavior;
- analytics attribution.

This is the first feature to build.

### Explicit-audience campaign

Upload or paste a bounded list of full 64-character pubkeys. The platform
validates, deduplicates, estimates eligibility, creates a holdout, and schedules
delivery.

This enables controlled creator programs and support outreach before automated
behavioral segmentation exists.

### New-user guidance

Approved examples:

- zero follows after a reasonable onboarding interval;
- incomplete first meaningful action;
- explain bells after the person follows creators.

Avoid generic "come back" messages.

### Followed-creator recap

Tell a person that creators they already follow have posted since their last
meaningful session. Prefer one recap over several individual pushes.

### Creator support

Approved examples:

- first meaningful comment;
- first repeat viewer;
- an older Vine finding a new audience;
- a real unanswered conversation;
- an invitation to a relevant creator program.

Never punish inactivity or describe declining metrics as personal failure.

## Explicitly out of MVP

- "Send to everyone."
- Creator-authored broadcasts.
- Automatic or AI-generated campaign copy.
- Arbitrary SQL entered by campaign authors.
- Segments defined only as "high value," "likely to churn," or another opaque
  score.
- FCM topic fan-out for private behavioral segments.
- Promotions without separate category control.
- Delivery to devices with unknown timezone.
- Campaigns without an expiry or stop condition.

## Proposed repository layout

Repository: `divine-engagement`

```text
src/
  app/                 # Internal web UI
  api/                 # Hono routes and request validation
  auth/                # Cloudflare Access identity and role checks
  campaigns/           # Campaign lifecycle and revisions
  segments/            # Approved segment definitions and resolvers
  experiments/         # Holdout assignment and outcome attribution
  workflows/           # Durable campaign orchestration
  queues/              # Recipient fan-out consumers and DLQ tools
  push/                # Typed internal push-service client
  audit/               # Append-only audit events
  db/                  # D1 queries and migrations
test/
migrations/
wrangler.jsonc
AGENTS.md
README.md
```

Suggested runtime:

- Cloudflare Worker with Hono for the API and internal web app.
- Cloudflare Access in front of the entire application.
- Validate the `Cf-Access-Jwt-Assertion` header in the Worker; do not trust the
  presence of the Access cookie alone.
- D1 for campaign definitions, revisions, approved segment definitions,
  experiments, delivery summaries, and audit events.
- Cloudflare Workflows for scheduled campaign state and durable steps.
- Cloudflare Queues for bounded recipient fan-out, retries, and backpressure.
- A dead-letter queue for delivery work that exceeds retry policy.
- R2 later for large import files and archived recipient-level exports if D1
  volume requires it.

Cloudflare Queues are at-least-once delivery. Every recipient delivery must
therefore have an idempotency key and a unique database constraint.

## System boundary

```text
                         Cloudflare Access
                                |
                                v
                  engagement.admin.divine.video
                 +-----------------------------+
                 | divine-engagement           |
                 | UI + API + D1               |
                 | Workflows + Queues + audit  |
                 +--------------+--------------+
                                |
                  approved segment definition
                                |
               +----------------+----------------+
               |                                 |
               v                                 v
       analytics/FunnelCake              divine-push-service
       audience resolution               eligibility + delivery
               |                                 |
               +---------- full pubkeys ---------+
                                                 |
                                                 v
                                                FCM
                                                 |
                                                 v
                                           Divine devices
```

## Repository changes

### New: `divine-engagement`

Owns:

- internal user interface;
- campaign and revision state;
- approved segment catalog;
- audience imports;
- approval workflow;
- scheduling and orchestration;
- experiments and holdouts;
- delivery summaries;
- audit history;
- push-service internal client.

Does not own:

- Firebase credentials;
- FCM registration tokens;
- behavioral source-of-truth data;
- mobile notification permission;
- social-event notification logic.

### Existing: `divine-push-service`

Add a narrow authenticated internal API for:

- delivery eligibility;
- test delivery;
- production campaign delivery;
- aggregate result reporting.

It must enforce:

- marketing consent;
- device validity;
- quiet hours;
- global and category frequency caps;
- campaign expiry;
- idempotency;
- allowed payload and tap-target shapes.

The API must never return FCM tokens.

The public HTTP surface remains only `/health`. Internal campaign endpoints
must be separately authenticated and network-restricted where practical.

### Existing: `divine-mobile`

Extend encrypted device registration with:

- IANA timezone, for example `Pacific/Auckland`;
- locale;
- platform;
- app version;
- a stable opaque installation identifier if the privacy review approves it;
- device metadata schema version.

Add:

- a separate marketing/recommendations notification preference;
- updated settings copy explaining categories;
- campaign tap-target handling;
- notification delivered/opened/destination-reached attribution;
- periodic refresh when timezone changes or registration metadata becomes
  stale.

Do not infer timezone from IP or locale.

### Existing: analytics/FunnelCake

Provide approved, versioned audience resolvers for behavioral segments.

Each resolver must return:

- full recipient pubkey;
- segment version;
- reason code;
- source timestamp;
- optional last-meaningful-activity timestamp;
- no FCM token or push-service secret.

The source of truth for "last active" must be defined before building
re-engagement segments. Token registration time is not app engagement.

### Existing: `divine-admin-dashboard`

Add a link card:

- title: Engagement
- URL: `https://engagement.admin.divine.video`
- description: plan, approve, schedule, and review respectful engagement
  campaigns.

The central dashboard remains a directory. Do not move campaign logic into it.

### Shared context and infrastructure

On repository creation:

- add `divine-engagement` to `divine-context/PROJECTS.md` the same day;
- document it in `divine-context/ARCHITECTURE.md`;
- add Cloudflare Access policy and deployment ownership;
- document secrets and service-to-service credentials without committing them;
- add the normal review-team mapping if the repository risk warrants a
  non-default team.

## Campaign lifecycle

```text
draft
  -> estimating
  -> ready_for_test
  -> awaiting_approval
  -> approved
  -> scheduled
  -> delivering
  -> completed

Any pre-completion state may move to:
  -> paused
  -> cancelled
  -> expired
  -> failed
```

Rules:

- only drafts are directly editable;
- submitting for approval creates an immutable revision;
- approval applies to one exact revision;
- changes to copy, audience, destination, timing, expiry, holdout, or cap policy
  invalidate approval;
- scheduling requires approval;
- delivery rechecks campaign status before every batch;
- cancellation prevents new sends but does not pretend already-sent pushes can
  be recalled;
- every transition appends an audit event.

## Data model

### `campaigns`

```text
id
name
category
status
current_revision_id
created_by
created_at
updated_at
```

### `campaign_revisions`

```text
id
campaign_id
revision_number
title
body
tap_target_type
tap_target_value
segment_definition_id
scheduled_at
expires_at
holdout_basis_points
frequency_policy_json
motivation
success_metric
guardrail_metric
created_by
created_at
```

### `segment_definitions`

```text
id
name
version
resolver_type
resolver_config_json
human_description
enabled
created_by
approved_by
created_at
```

`resolver_config_json` is validated against a server-owned schema. It is not
arbitrary SQL.

### `campaign_recipients`

```text
campaign_revision_id
recipient_pubkey
reason_code
source_timestamp
assignment              # treatment or holdout
eligibility_status
not_before
attempt_count
terminal_status
created_at
updated_at
```

Unique key:

```text
(campaign_revision_id, recipient_pubkey)
```

If delivery becomes per-device, the durable job may add an opaque
push-service-generated device reference. It must never store the FCM token.

### `delivery_attempts`

```text
id
campaign_revision_id
recipient_pubkey
idempotency_key
attempt_number
requested_at
result_code
retry_after
provider_message_id_hash
completed_at
```

Never store raw FCM tokens or sensitive provider responses.

### `audit_events`

```text
id
campaign_id
campaign_revision_id
actor_identity
action
reason
metadata_json
occurred_at
```

Audit events are append-only through application code.

## Internal push-service contract

The exact contract needs a security and ownership spike before implementation.
The proposed shape is:

### Eligibility/plan

```http
POST /internal/v1/campaigns/eligibility
```

Request:

```json
{
  "campaignRevisionId": "uuid",
  "category": "engagement",
  "expiresAt": "2026-08-01T12:00:00Z",
  "recipients": ["<full-pubkey>"]
}
```

Response per pubkey:

```json
{
  "recipientPubkey": "<full-pubkey>",
  "status": "eligible | opted_out | no_device | stale_device | capped | deferred",
  "notBefore": "2026-08-01T19:00:00Z"
}
```

This response must not claim exact delivery because eligibility is rechecked at
send time.

### Deliver

```http
POST /internal/v1/campaigns/deliver
```

Request:

```json
{
  "campaignRevisionId": "uuid",
  "idempotencyKey": "uuid:recipient-pubkey",
  "recipientPubkey": "<full-pubkey>",
  "category": "engagement",
  "title": "Three creators you follow posted",
  "body": "See what they made this week.",
  "tapTarget": {
    "type": "route",
    "value": "/following/new"
  },
  "expiresAt": "2026-08-01T12:00:00Z"
}
```

Response:

```json
{
  "status": "delivered | deferred | suppressed | permanent_failure | retryable_failure",
  "reason": "quiet_hours | opted_out | capped | no_device | provider_error",
  "retryAfter": "2026-08-01T19:00:00Z"
}
```

Questions to resolve in the spike:

1. Is timezone policy evaluated at pubkey or device level?
2. If one pubkey has devices in different timezones, may safe devices receive
   while quiet devices defer?
3. If delivery is per-device, how does `divine-engagement` hold an opaque
   device job without gaining access to the FCM token?
4. Where does durable deferred-device state live?
5. Which service owns the final frequency-cap ledger?

Recommended principle: `divine-push-service` remains the final enforcement
point even if `divine-engagement` performs an earlier estimate.

## Quiet hours

Hard rule for marketing and engagement categories:

```text
do not deliver when recipient local time is >= 21:00 or < 07:00
```

Requirements:

- use IANA timezone identifiers, not UTC offsets;
- handle daylight-saving transitions through a maintained timezone library;
- recheck local time at delivery, not only at campaign creation;
- defer to the next local 07:00 when possible;
- respect campaign expiry; an expired message is dropped rather than delivered
  late;
- never allow an author or approver to bypass quiet hours;
- record a machine-readable reason when deferred or dropped;
- test DST gaps, DST overlaps, timezone changes, and invalid timezone input.

Default unknown-timezone behavior: suppress the campaign delivery.

## Frequency policy

Initial global policy:

| Category | Initial maximum |
| --- | --- |
| Staff-authored promotion | 1 per person per 7 days |
| Personalized re-engagement | 1 per person per 7 days |
| Product education | 2 during onboarding, then stop |
| Digest | Weekly by default |
| All non-social campaigns combined | 2 per person per 7 days |

Campaign authors may choose a stricter cap, never a looser one.

The cap ledger should be keyed on recipient, category, and successful delivery.
A failed provider request must not consume the person's cap.

## Segment model

Segments are approved code-backed resolvers, not ad hoc database queries.

Each segment definition has:

- a stable name and version;
- a human-readable explanation;
- typed parameters;
- explicit exclusions;
- an owning team;
- test fixtures;
- a maximum audience size;
- a freshness requirement;
- a disable switch.

MVP resolvers:

1. `internal_test_pubkeys`
2. `explicit_pubkey_list`

Next resolvers, only after source-of-truth decisions:

3. `zero_follows_after_onboarding`
4. `first_post_without_second_post`
5. `followed_creators_posted_since_last_active`
6. `inactive_for_days`
7. `creator_with_unanswered_comments`
8. `opted_into_program`

Every resolved recipient carries a reason code so the UI can answer "why is
this person included?"

## Approval and access

Cloudflare Access protects the application boundary. The Worker also validates
the Access JWT and maps identity to application roles.

Required controls:

- deny by default;
- production roles managed outside application code;
- authors cannot approve their own campaigns;
- two-person approval for any campaign above a configurable audience threshold;
- test recipients must be drawn from an allowlisted internal segment;
- no production delivery from preview deployments;
- service-to-service authentication between `divine-engagement` and
  `divine-push-service`;
- secrets stored in platform secret storage;
- immutable audit event for every privileged action;
- emergency pause available to a small operator group;
- rate limit internal APIs even though they are authenticated.

## Tap targets and copy

The campaign platform exposes an allowlist of typed destinations, not a free
form data payload.

Initial destination types:

- Divine app route;
- specific video address;
- creator profile;
- following/new recap;
- notification settings;
- approved HTTPS help/program page.

The UI should render platform previews and execute a test-tap check before
approval.

Copy requirements:

- non-empty title and body;
- reason for receipt is apparent;
- no unsupported urgency;
- no claim of an interaction that did not occur;
- no placeholder variables unresolved;
- no raw Nostr identifiers in user-facing copy;
- brand review for reusable templates;
- exact revision stored with the campaign.

## Measurement

Primary campaign measures:

- provider-accepted delivery;
- notification open;
- destination reached;
- meaningful action completed;
- seven-day and 28-day retention lift against holdout.

Trust guardrails:

- marketing opt-out;
- OS notification permission loss where measurable;
- mute/unbell;
- repeated non-open;
- app uninstall indicator where legitimately available;
- sends per person;
- duplicate delivery;
- quiet-hour violation;
- wrong-destination report.

Do not describe provider acceptance as confirmed device display.

Every production campaign must define:

- one primary meaningful-action metric;
- one trust guardrail;
- an attribution window;
- a holdout percentage;
- a stop condition.

## Delivery orchestration

Suggested flow:

1. Author creates a draft.
2. Platform resolves or imports a preview audience and displays counts.
3. Author sends a test campaign to internal recipients.
4. Author submits an immutable revision for approval.
5. Approver reviews copy, destination, audience, exclusions, schedule, expiry,
   holdout, caps, and guardrails.
6. On approval and schedule, a Cloudflare Workflow starts.
7. Workflow resolves the final versioned audience.
8. Platform deterministically assigns holdout/treatment.
9. Platform inserts recipient rows with a unique campaign-recipient key.
10. Workflow publishes bounded recipient jobs to a Queue.
11. Queue consumer calls `divine-push-service`.
12. Push service rechecks consent, quiet hours, caps, expiry, and idempotency.
13. Deferred jobs are scheduled for `retryAfter`.
14. Retryable failures use bounded backoff.
15. Permanent failures become terminal.
16. Exhausted retries enter the dead-letter queue.
17. Workflow finalizes aggregate results without erasing recipient history.

Queue delay is not the long-term campaign scheduler. Cloudflare Queues currently
support per-message delay only up to 24 hours; use a Workflow `sleepUntil` for
future campaign schedule points and Queue delay/backoff only for bounded
recipient work.

## API surface for `divine-engagement`

Indicative routes:

```text
GET    /api/me
GET    /api/campaigns
POST   /api/campaigns
GET    /api/campaigns/:id
POST   /api/campaigns/:id/revisions
POST   /api/campaigns/:id/estimate
POST   /api/campaigns/:id/test
POST   /api/campaigns/:id/submit
POST   /api/campaigns/:id/approve
POST   /api/campaigns/:id/reject
POST   /api/campaigns/:id/schedule
POST   /api/campaigns/:id/pause
POST   /api/campaigns/:id/resume
POST   /api/campaigns/:id/cancel
GET    /api/campaigns/:id/results
GET    /api/campaigns/:id/audit
GET    /api/segments
POST   /api/imports/pubkeys
GET    /api/operations/dead-letters
POST   /api/operations/global-pause
```

Every mutation requires:

- authenticated actor;
- role check;
- CSRF-safe request pattern;
- idempotency key where retry is plausible;
- audit event;
- optimistic concurrency or revision precondition.

## Testing strategy

### Unit tests

- campaign state transitions;
- revision immutability;
- role matrix;
- self-approval denial;
- segment parameter validation;
- deterministic holdout assignment;
- idempotency-key construction;
- quiet-hour boundaries;
- timezone and DST behavior;
- cap evaluation;
- expiry;
- copy and tap-target validation.

### Integration tests

- D1 migrations and unique constraints;
- Workflow restart/resume behavior;
- Queue at-least-once duplicate delivery;
- dead-letter behavior;
- push-service authentication;
- push-service retryable, deferred, suppressed, and terminal results;
- cancellation while jobs are queued;
- no production send from preview/staging.

### End-to-end tests

- internal test campaign on Android and iOS;
- tap opens exact destination from foreground, background, and terminated app;
- campaign scheduled across at least three IANA timezones;
- no delivery inside 21:00-07:00;
- DST transition;
- opt-out before scheduled delivery;
- pause/cancel during fan-out;
- duplicate queue delivery produces one push;
- holdout receives no campaign;
- audit history names every actor and transition.

## Rollout

### Phase 0: decisions and security spike

Deliverables:

- confirm repository name and owning team;
- select launch use cases;
- define marketing consent;
- define source of truth for last meaningful activity;
- decide per-user versus per-device timezone policy;
- design the internal push-service contract;
- threat model and authorization model;
- define initial caps and stop conditions.

No production sending.

### Phase 1: internal test sends

Build:

- new repository scaffold;
- Cloudflare Access JWT validation and roles;
- D1 campaign/audit schema;
- compose and preview UI;
- internal-test segment;
- narrow push-service test-delivery endpoint;
- device timezone registration;
- quiet-hour and expiry enforcement;
- one-device test delivery;
- central admin-dashboard link.

Exit criteria:

- Android and iOS rendering verified;
- exact tap routing verified;
- quiet hours tested in multiple timezones;
- complete audit history;
- no raw token exposure.

### Phase 2: explicit audiences

Build:

- bounded pubkey import;
- immutable revisions and approval;
- campaign Workflows;
- Queue fan-out and dead-letter handling;
- pause/cancel;
- idempotency and retry;
- consent and frequency caps;
- holdouts;
- aggregate results.

Exit criteria:

- controlled production pilot;
- no duplicate sends;
- no quiet-hour violations;
- stop condition and emergency pause tested.

### Phase 3: approved behavioral segments

Build only the first two product-approved resolvers. Add:

- segment versioning;
- audience freshness;
- reason codes;
- meaningful-action analytics;
- campaign comparisons against holdout;
- automatic suppression after repeated non-engagement.

### Later: creator community messaging

Treat creator broadcasts as a separate product phase with:

- explicit audience join;
- some/all/none notification control;
- creator send allowance;
- moderation and reporting;
- mute and leave;
- prompts and replies;
- creator reach and interaction insights.

Do not expose staff campaign infrastructure directly to creators.

## Open product questions

1. Which two user problems and one creator-support problem launch first?
2. Is marketing consent explicit opt-in, or a separately exposed category that
   defaults on for already-authorized users?
3. Is 21:00-07:00 fixed, or may users choose a wider quiet window?
4. What happens when timezone is missing: suppress, or use another conservative
   policy?
5. Are campaign preferences per pubkey, per device, or layered?
6. What is the authoritative definition of "last active" and "meaningful
   session"?
7. Which roles may author, approve, operate, and audit?
8. What audience size requires two-person approval?
9. Who signs off copy and reusable templates?
10. What total weekly interruption budget is shared across marketing, product,
    creator, and recommendation messages?
11. What opt-out or permission-loss threshold automatically pauses a campaign?
12. How long is recipient-level campaign history retained?
13. Which team owns the campaign platform operationally?
14. What privacy review is required for device metadata and engagement
    segmentation?
15. When, if ever, do creators gain opted-in broadcast tools?

## Open technical questions

1. Does the campaign platform remain entirely on Cloudflare, or should durable
   recipient history use existing GKE Postgres?
2. Is D1 sufficient for expected recipient-attempt volume and retention?
3. Should large imports and old recipient detail move to R2?
4. How does Cloudflare securely reach the GKE push service?
5. Should the internal delivery API validate Cloudflare Access service tokens,
   mTLS, signed requests, or a combination?
6. Which service owns durable deferred per-device jobs?
7. Can the current FCM library expose stable provider result codes needed for
   retry classification?
8. How are delivery and open events joined without exposing FCM tokens?
9. How are mobile timezone changes refreshed promptly but efficiently?
10. Which environments use separate Firebase projects and test audiences?

## Decisions needed before scaffolding

The following decisions materially change the implementation and should be made
before creating the repository:

1. Confirm the repository and product name. Recommendation:
   `divine-engagement`.
2. Confirm the hosting model. Recommendation: Cloudflare Worker + D1 +
   Workflows + Queues for the MVP.
3. Confirm the first release is internal test sends followed by explicit
   pubkey-list campaigns.
4. Confirm unknown timezone means suppress.
5. Confirm a separate marketing/recommendations preference is required before
   production campaigns.
6. Name the source and owner for meaningful activity data.
7. Name the product owner, engineering owner, and production approver group.

## Research and platform references

- YouTube notification limits and personalized timing:
  https://support.google.com/youtube/answer/7389684
- YouTube creator bell reach metrics:
  https://support.google.com/youtube/answer/9336507
- Instagram broadcast-channel opt-in model:
  https://about.fb.com/news/2023/02/instagram-broadcast-channels-creators-deepen-connections-with-followers/
- Instagram prompts, replies, and creator insights:
  https://about.fb.com/news/2024/12/get-closer-to-your-community-with-replies-prompts-and-insights/
- Notification decision optimization research:
  https://arxiv.org/abs/2202.08812
- Cloudflare Access JWT validation:
  https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/
- Cloudflare Workflows scheduling and retries:
  https://developers.cloudflare.com/workflows/build/sleeping-and-retrying/
- Cloudflare Queues retry and delay behavior:
  https://developers.cloudflare.com/queues/configuration/batching-retries/
- Cloudflare Queues dead-letter queues:
  https://developers.cloudflare.com/queues/configuration/dead-letter-queues/
- Cloudflare D1 limits:
  https://developers.cloudflare.com/d1/platform/limits/
- Firebase token-management guidance:
  https://firebase.google.com/docs/cloud-messaging/manage-tokens
- Firebase topic-messaging guidance:
  https://firebase.google.com/docs/cloud-messaging/topic-messaging
