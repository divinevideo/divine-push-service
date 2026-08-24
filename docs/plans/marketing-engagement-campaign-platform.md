# Marketing engagement notifications — archived pointer

Status: archived. Superseded by the canonical plan in `divine-engagement`.
Original draft date: 2026-08-05

This repository briefly carried a full copy of the engagement-notification
discovery draft. It has been reduced to this pointer. A second copy in the
repository most likely to implement push behaviour is the copy that goes stale,
and a stale copy here points an implementer at a design that was rejected.

## Where the current material lives

- **Plan:** [`divine-engagement`, `docs/plans/marketing-engagement-campaign-platform.md`](https://github.com/divinevideo/divine-engagement/blob/main/docs/plans/marketing-engagement-campaign-platform.md)
- **Delivery contract:** [`divine-engagement`, `docs/push-service-contract.md`](https://github.com/divinevideo/divine-engagement/blob/main/docs/push-service-contract.md)

## What this repository's role actually is

The proposed delivery direction for `divine-push-service` is recorded in the
second link above. It matters because the draft archived here described the
opposite direction, and that direction was rejected:

`divine-push-service` runs on GKE in the `push` namespace with **no public
ingress**. Its current HTTP routes are `GET /health` and `GET /metrics`; it is
therefore **not** an API that `divine-engagement` calls. Under the proposed
contract, push-service would poll:

```text
GET  /api/internal/deliveries/pending   -> lease a batch of approved copy
POST /api/internal/deliveries/results   -> report what happened
```

Consent, quiet hours, and frequency caps would remain push-service
responsibilities and be applied after the lease. These controls and the poller
are not implemented yet.

Any `POST /internal/v1/campaigns/*` ingress surface described in earlier drafts
is **not** to be built. Adding ingress here was the wrong turn the contract
document exists to record.

## Why the brief was archived

Keeping the full discovery draft here would create a second copy that could
drift from the canonical plan. Defects found while reviewing the duplicate are
[tracked against the canonical plan](https://github.com/divinevideo/divine-engagement/issues/2)
and should not be copied or fixed here.
