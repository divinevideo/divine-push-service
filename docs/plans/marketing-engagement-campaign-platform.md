# Marketing engagement notifications — archived pointer

Status: archived. Superseded by the canonical plan in `divine-engagement`.
Date: 2026-08-05

This repository briefly carried a full copy of the engagement-notification
discovery draft. It has been reduced to this pointer. A second copy in the
repository most likely to implement push behaviour is the copy that goes stale,
and a stale copy here points an implementer at a design that was rejected.

## Where the current material lives

- **Plan:** [`divine-engagement`, `docs/plans/marketing-engagement-campaign-platform.md`](https://github.com/divinevideo/divine-engagement/blob/main/docs/plans/marketing-engagement-campaign-platform.md)
- **Delivery contract:** [`divine-engagement`, `docs/push-service-contract.md`](https://github.com/divinevideo/divine-engagement/blob/main/docs/push-service-contract.md)

## What this repository's role actually is

The contract that governs `divine-push-service` is the second link above. It
matters because the draft archived here described the opposite direction, and
that direction was rejected:

`divine-push-service` runs on GKE in the `push` namespace with **no public
ingress** — its entire HTTP surface is `GET /health`. It is therefore **not** an
API that `divine-engagement` calls. Push-service **polls**:

```text
GET  /api/internal/deliveries/pending   -> lease a batch of approved copy
POST /api/internal/deliveries/results   -> report what happened
```

Consent, quiet hours, and frequency caps are applied on this side, after the
lease.

Any `POST /internal/v1/campaigns/*` ingress surface described in earlier drafts
is **not** to be built. Adding ingress here was the wrong turn the contract
document exists to record.

## Why the brief was archived rather than kept

The discovery draft also carried defects that were reported during review of
this PR (divinevideo/divine-push-service#38) and belong to the canonical copy,
not to a duplicate:

1. The expiry example contradicts the expiry rule — an `expiresAt` of `12:00:00Z`
   paired with a `notBefore`/`retryAfter` of `19:00:00Z`, a deferral seven hours
   past an expiry the same document says causes a drop rather than a late send.
2. The frequency caps have no precedence rule: the per-category maxima sum to
   roughly 5 per 7 days against a combined non-social cap of 2, and product
   education alone consumes the entire budget during onboarding.
3. FunnelCake is named as the source of behavioural and engagement facts. It is
   the Nostr backend — relay plus REST over ClickHouse — and stores events, not
   app-engagement data. Either a different source is intended or this is
   unscoped new work for FunnelCake.
4. The unknown-timezone policy is simultaneously decided (suppress, by default),
   excluded from the MVP, and listed as an open question.
5. Unit mismatch: `holdout_basis_points` is stored, while the requirement is
   written as "a holdout percentage".

These are tracked against the canonical plan. Do not fix them here.
