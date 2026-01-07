# diVine Push Service

Push notification service for the [diVine](https://divine.video) mobile app.

Implements [NIP-XX Push Notifications](docs/nip-xx-push-notifications.md) to deliver real-time Nostr event notifications via Firebase Cloud Messaging (FCM).

## Features

- **Encrypted registration**: NIP-44 encrypted push tokens (plaintext rejected)
- **User preferences**: Enable/disable notifications by event kind (3083)
- **Supported notifications**:
  - Likes (kind 7)
  - Comments/replies (kind 1)
  - Follows (kind 3)
  - Mentions (kind 1 with p-tags)
  - Reposts (kind 16)
- **Replay protection**: 7-day horizon ignores old events
- **Deduplication**: Prevents duplicate notifications

## Protocol

Three Nostr event kinds:

| Kind | Purpose |
|------|---------|
| 3079 | Register push token (encrypted) |
| 3080 | Deregister push token (encrypted) |
| 3083 | Update notification preferences |

Events must include a `p` tag with the service's public key and use NIP-44 encryption.

## Quick Start

### Prerequisites

- Rust 1.85+
- Redis
- Firebase project with FCM enabled

### Development

```bash
# Start Redis
docker run -d -p 6379:6379 redis:7-alpine

# Configure environment
export NOSTR_PUSH__SERVICE__PRIVATE_KEY_HEX="<your_service_private_key>"
export NOSTR_PUSH__REDIS__URL="redis://localhost:6379"

# Place Firebase credentials
cp firebase-service-account.json firebase-service-account-divine.json

# Run
cargo run
```

### Docker

```bash
docker compose up -d
```

## Configuration

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `NOSTR_PUSH__SERVICE__PRIVATE_KEY_HEX` | Yes | Service's Nostr private key (hex) |
| `NOSTR_PUSH__REDIS__URL` | Yes | Redis connection URL |
| `RUST_LOG` | No | Log level (default: `info`) |

### Config Files

- `config/settings.yaml` - Default settings
- `config/settings.production.yaml` - Production overrides

## Architecture

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  diVine App │────▶│ Nostr Relays│◀────│ Push Service│
└─────────────┘     └─────────────┘     └──────┬──────┘
                                               │
                    ┌─────────────┐     ┌──────▼──────┐
                    │   Firebase  │◀────│    Redis    │
                    │     FCM     │     │ (tokens)    │
                    └──────┬──────┘     └─────────────┘
                           │
                    ┌──────▼──────┐
                    │  Mobile     │
                    │  Device     │
                    └─────────────┘
```

1. **diVine app** registers push token via NIP-44 encrypted event to relays
2. **Push service** monitors relays, decrypts registration, stores token in Redis
3. **Push service** monitors relays for events matching user's subscriptions
4. **Push service** sends notification to Firebase FCM
5. **Firebase** delivers notification to mobile device

## API Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check |

## License

MIT
