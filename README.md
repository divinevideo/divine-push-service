# Divine Push Service

Push notification service for the [Divine](https://divine.video) mobile app. It is a Rust service that watches Nostr relays for events that should notify a user — likes, comments, reposts, mentions — and delivers them to registered devices through Firebase Cloud Messaging (FCM). Devices register encrypted push tokens over Nostr, so the app never needs an always-on relay connection to receive alerts.

The service implements a draft push-notification protocol; see [docs/nip-xx-push-notifications.md](docs/nip-xx-push-notifications.md) for the specification and [docs/developer-guide.md](docs/developer-guide.md) for the delivery internals.

## Features

- **Encrypted token registration** — push tokens are carried in NIP-44 encrypted Nostr events; plaintext registrations are rejected.
- **Notification delivery over FCM** — one incoming Nostr event produces exactly one visible banner, with per-platform payloads for Android (data-only) and iOS (APNS `aps.alert`).
- **User preferences** — users can opt in or out of notification kinds with a preferences event; sensible defaults apply otherwise.
- **Deduplication** — atomic Redis `SET NX EX` per-event locks ensure each event is delivered once, even across replicas.
- **Replay protection** — a configurable processing window (7 days by default) ignores stale events.
- **Token cleanup** — a background task prunes stale tokens (older than 90 days by default) once a day.
- **Optional allow-list** — `allowed_pubkeys` can restrict delivery to a specific set of recipients.

## Protocol

Devices talk to the service entirely through Nostr events addressed to the service's public key. Every such event carries a `p` tag with the service pubkey and NIP-44 encrypted content.

| Kind | Purpose |
|------|---------|
| 3079 | Register a push token (encrypted) |
| 3080 | Deregister a push token (encrypted) |
| 3083 | Update notification preferences (optional) |

Clients discover the service's public key from the `/health` endpoint (see [API](#api)) and use it both as the `p` tag and as the NIP-44 encryption target.

## Notification types

The service subscribes to trigger events on its relay and notifies the tagged recipient:

| Type | Event kind | Trigger |
|------|-----------|---------|
| Like | 7 | Reaction to a user's note (NIP-25) |
| Comment | 1111 | NIP-22 comment on a user's video or article |
| Reply | 1 | Reply to a user's note |
| Mention | 1 | Note mentioning a user |
| Repost | 16 | Repost of a user's note (NIP-18) |
| New post | 34236 | A creator the user subscribed to ("belled") published a video |

New-post notifications are the one type not anchored to a `p` tag on the trigger event. Recipients come from the subscriber's own NIP-51 list (kind 30000, `d=notify`), so the service resolves them from a Redis reverse index rather than from the video. They are rate-limited to one push per (subscriber, creator) per hour, and fan-out is paged and delivered with bounded concurrency so one popular creator cannot force one unbounded Redis read or sequential delivery loop. The in-app feed is not throttled. See [the protocol doc](docs/nip-xx-push-notifications.md) for the list shape.

Divine video comments are NIP-22 `kind:1111` and notify both the root video author and the direct parent author (deduplicated when they coincide). Follows (kind 3) are defined in the protocol and subscribed to, but new-follow notifications are **not currently emitted** — that requires diffing contact-list state, which is not yet implemented.

Each FCM message carries a stable `data` payload with routing and presentation fields. Routing to the correct video uses the authoritative addressable coordinate from the triggering event, never a coordinate synthesized from the recipient's pubkey. The full payload contract is documented in the [developer guide](docs/developer-guide.md#fcm-payload-format).

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌──────────────┐
│  Divine app │────▶│ Nostr relays │◀────│ Push service │
└─────────────┘     └──────────────┘     └──────┬───────┘
                                                │
                    ┌──────────────┐     ┌──────▼───────┐
                    │  Firebase    │◀────│    Redis     │
                    │     FCM      │     │ (tokens,     │
                    └──────┬───────┘     │  dedup)      │
                           │             └──────────────┘
                    ┌──────▼───────┐
                    │ Mobile device│
                    └──────────────┘
```

1. The Divine app fetches its FCM token, NIP-44 encrypts it, and publishes a kind 3079 event tagged to the service.
2. The push service receives the event over its relay subscription, decrypts the token, and stores it in Redis keyed by the user's pubkey.
3. The service watches the relay for trigger events (likes, comments, reposts, mentions) referencing registered users.
4. For each match it checks dedup and the recipient's preferences, then sends a data message to Firebase FCM.
5. Firebase delivers the notification to the device.

The service is a single async binary running four cooperating tasks: a Nostr listener, an event handler, the token-cleanup service, and an HTTP server for health checks. Those tasks are supervised: if one ends unexpectedly, the others are cancelled and the process exits non-zero, so a pod whose delivery pipeline has died is restarted instead of staying in service. It is single-app — one Firebase project, one relay — built with `axum`, `tokio`, `nostr-sdk`, and `redis`.

## Getting started

### Prerequisites

- Rust 1.85+
- Redis
- A Firebase project with FCM enabled, and a service-account credentials file

### Development

```bash
# Start Redis
docker run -d -p 6379:6379 redis:7-alpine

# Provide the service's Nostr key (used for NIP-44 decryption)
export NOSTR_PUSH__SERVICE__PRIVATE_KEY_HEX="<service_private_key_hex>"

# Place Firebase credentials where the development config expects them
cp <your-service-account>.json firebase-service-account-divine.json

# Run (APP_ENV defaults to "development", loading config/settings.development.yaml)
cargo run
```

### Docker

```bash
# Set SERVICE_PRIVATE_KEY in your environment or a .env file first
docker compose up -d
```

`docker compose` runs the service (with `APP_ENV=production`) alongside a Redis instance and mounts `firebase-service-account-divine.json` for FCM credentials.

### Tests

```bash
cargo test          # integration and unit tests (some require a running Redis)
cargo clippy --all-targets --all-features
cargo fmt --all -- --check
```

## Configuration

Configuration is layered: a YAML file selected by `APP_ENV`, then environment variables that override any value.

### Config files

`APP_ENV` selects the file under `config/`:

- `APP_ENV=development` (the default) loads `config/settings.development.yaml`.
- `APP_ENV=production` loads `config/settings.yaml`.
- Any other value loads `config/settings.<APP_ENV>.yaml`.

These files set the relay (`wss://relay.divine.video`), profile relays, notification kinds, cleanup schedule, the Firebase project, and the listen address (`0.0.0.0:8000`).

### Environment variables

Any setting can be overridden with the `NOSTR_PUSH__` prefix and `__` as the nesting separator (for example `redis.url` becomes `NOSTR_PUSH__REDIS__URL`).

| Variable | Required | Description |
|----------|----------|-------------|
| `NOSTR_PUSH__SERVICE__PRIVATE_KEY_HEX` | Yes | Service's Nostr private key (hex), used for NIP-44 decryption |
| `NOSTR_PUSH__REDIS__URL` | No | Redis connection URL (overrides the config file) |
| `NOSTR_PUSH__NOSTR__RELAY_URL` | No | Nostr relay to subscribe to |
| `NOSTR_PUSH__NOSTR__EVENT_SILENCE_TIMEOUT_SECS` | No | Quiet period before resubscribing, then failing health if silence continues (default `300`) |
| `APP_ENV` | No | Selects the config file (default `development`) |
| `RUST_LOG` | No | Log level (default `info`) |

### Firebase credentials

The service authenticates to FCM per the app's `firebase` config:

- Set `credentials_path` to a service-account JSON file (used in development and Docker).
- Omit `credentials_path` to fall back to Application Default Credentials — for example GKE Workload Identity in production.

## Deployment

Production images are built and published by the `Build, Test & Push` GitHub Actions workflow (`.github/workflows/publish-and-release.yml`):

- On every push to `main`, the workflow runs tests, builds a single Docker image, and pushes it to the POC, Test, and Staging Google Artifact Registry environments using Workload Identity federation.
- Pushes to a `v*` tag (or a manual dispatch opting in) additionally publish to the Production registry.
- After publishing, it dispatches an `image-deploy` event to `divinevideo/divine-iac-coreconfig`, which drives the ArgoCD rollout to the selected environments.

The container is a multi-stage build on `debian:bookworm-slim` that bundles the release binary and the `config/` directory, and exposes port 8000.

## API

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check and service-key discovery. Clients read `pubkey` to discover the service key for registration and encryption. |

`/health` answers `200` while the delivery pipeline is alive:

```json
{
  "status": "ok",
  "pubkey": "<hex>",
  "tasks": { "nostr_listener": true, "event_handler": true }
}
```

If the Nostr listener or the event handler has died it answers `503` with
`"status": "degraded"` and that task set to `false`. Both the liveness and the
readiness probe point here, so a dead pipeline fails its probes rather than
serving `200` behind a healthy-looking pod.

## License

MIT

---

Part of [Divine](https://divine.video) — your playground for human creativity · [Brand guidelines](https://github.com/divinevideo/brand-guidelines)
