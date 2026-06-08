# Developer Guide

## Architecture Overview

divine-push-service is a single-app Nostr push notification service. It connects to Nostr relays, watches for events that should trigger notifications, and delivers them via Firebase Cloud Messaging (FCM).

```mermaid
sequenceDiagram
    participant App as Mobile App
    participant Relay as Nostr Relay
    participant Push as Push Service
    participant Redis
    participant FCM as Firebase FCM
    participant Device

    Note over App,Device: Token Registration
    App->>App: Get FCM token
    App->>App: NIP-44 encrypt token
    App->>Relay: Publish Kind 3079 (encrypted token, p-tag to push service)
    Relay->>Push: Event received via subscription
    Push->>Push: Decrypt NIP-44 content
    Push->>Redis: Store token for pubkey

    Note over App,Device: Notification Delivery
    Relay->>Push: New event (like, comment, follow, etc.)
    Push->>Redis: Check recipient has registered token
    Push->>Redis: Check dedup (SET NX EX)
    Push->>Redis: Check user preferences
    Push->>FCM: Send data-only message
    FCM->>Device: Push notification
```

## Event Kinds

| Kind | Direction | Purpose |
|------|-----------|---------|
| 3079 | Client → Relay → Service | Register FCM push token (NIP-44 encrypted) |
| 3080 | Client → Relay → Service | Deregister push token (NIP-44 encrypted) |
| 3083 | Client → Relay → Service | Update notification preferences (optional) |

See [NIP-XX Push Notifications](nip-xx-push-notifications.md) for the full protocol specification.

## Notification Types

The service watches for these event kinds and notifies the tagged recipient:

| Type | Event Kind | Trigger |
|------|-----------|---------|
| Like | 7 | Reaction to user's note (p-tag) |
| Comment | 1 | Reply to user's note (p-tag, with e-tag reference) |
| Follow | 3 | New contact list including user (p-tag) |
| Mention | 1 | Note mentioning user (p-tag, no e-tag reference) |
| Repost | 16 | Repost of user's note (p-tag) |

> **Note:** Follow (kind 3) is defined but **not currently emitted** — the handler skips kind 3 because new-follow detection requires diffing contact-list state, which is not yet implemented. Likes, comments, mentions, and reposts are the types actually delivered today.

## FCM Payload Format

The FCM message carries **no top-level `notification` field** — the `data` map below is always present and is identical in shape for every notification type (only the `title`/`body` strings differ). Per-platform delivery then diverges so that **one incoming push produces exactly one visible banner**:

- **Android** — data-only (`notification` and `android` unset). Android does not auto-display data messages, so the app renders the single banner itself from the `data` fields.
- **iOS** — the service attaches an APNS override: `aps.alert` (title/body) + `mutable-content: 1`, push-type `alert`, priority 10. The OS presents the single banner; a Notification Service Extension (if shipped) uses `mutable-content` to *enrich* that same banner, never to create a second one. `content-available` is deliberately omitted — see [Avoiding duplicate banners](#avoiding-duplicate-banners).

```json
{
  "data": {
    "type": "Like",
    "eventId": "abc123...",
    "title": "New like",
    "body": "Alice liked your post",
    "senderPubkey": "def456...",
    "senderName": "Alice",
    "receiverPubkey": "789abc...",
    "receiverNpub": "npub1...",
    "eventKind": "7",
    "timestamp": "1712345678",
    "referencedEventId": "fedcba..."
  }
}
```

### Fields

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | `Like`, `Comment`, `Follow`, `Mention`, or `Repost` |
| `eventId` | hex | The Nostr event that triggered the notification |
| `title` | string | Human-readable title (e.g. "New like") |
| `body` | string | Human-readable body (e.g. "Alice liked your post") |
| `senderPubkey` | hex | Pubkey of the user who triggered the event |
| `senderName` | string | Display name or truncated npub of the sender |
| `receiverPubkey` | hex | Pubkey of the notification recipient |
| `receiverNpub` | bech32 | Bech32-encoded npub of the recipient |
| `eventKind` | string | Nostr event kind as string (e.g. "7") |
| `timestamp` | string | Unix timestamp of the event as string |
| `referencedEventId` | hex | (optional) The event being reacted to or replied to |

### iOS APNS shape

For a like, the APNS override the service emits is:

```json
{
  "aps": {
    "alert": { "title": "New like", "body": "Alice liked your post" },
    "mutable-content": 1
  },
  "type": "Like",
  "eventId": "abc123...",
  "...": "remaining data fields (title/body live in aps.alert, not duplicated here)"
}
```

Headers: `apns-push-type: alert`, `apns-priority: 10`.

A *silent/background* push — a data message with neither `title` nor `body` — instead uses `aps.content-available: 1`, push-type `background`, priority 5. The current notification types always carry `title`/`body`, so this background shape is not emitted today.

### Avoiding duplicate banners

`content-available: 1` is intentionally **absent** from alert pushes. It is iOS's background-update flag: it wakes the app's background isolate, which would build a **second, local** banner on top of the OS-presented `aps.alert` — the duplicate-banner bug ([divine-push-service#20](https://github.com/divinevideo/divine-push-service/issues/20)). An `aps.alert` push is delivered reliably to **terminated** iOS apps *without* `content-available` (that flag matters only for *silent* pushes, which iOS throttles when the app is terminated), so omitting it costs no delivery reliability.

The contract is mirrored on the client ([divine-mobile#4760](https://github.com/divinevideo/divine-mobile/pull/4760)): the app renders a local banner **only** when the message has no OS-presented notification (`message.notification == null`, i.e. the Android data-only case). When iOS surfaces the `aps.alert` as `RemoteMessage.notification`, the client suppresses its local render. Result: **one push → one banner** across foreground, background, and terminated states.

### Client Handling

- **Android**: data-only — the app creates and displays the notification via `onMessageReceived` / background handler.
- **iOS**: the OS presents the `aps.alert`; an optional Notification Service Extension enriches it via `mutable-content`. The app must **not** create a separate local notification for these.
- **Foreground**: iOS does not OS-present in the foreground, so the app is the sole renderer; Android likewise renders once.
- **Taps**: tapping the OS-presented banner routes via the platform notification-open callbacks (e.g. `onMessageOpenedApp` / `getInitialMessage`) using the `data` fields; routing does not depend on `content-available`.

## Service Discovery

The push service exposes its public key via the `/health` endpoint:

```
GET /health
```

```json
{
  "status": "ok",
  "pubkey": "abc123..."
}
```

Clients use this pubkey to:
- Set the `p` tag on Kind 3079/3080/3083 events
- Encrypt the NIP-44 content to the service's key

## Deduplication

The service uses atomic Redis `SET NX EX` per-event keys to prevent duplicate notifications across multiple replicas. Each event is claimed exactly once with a 7-day TTL.

## User Preferences

Users can optionally send a Kind 3083 event to control which notification types they receive. The decrypted content is:

```json
{ "kinds": [1, 3, 7, 16] }
```

This is a list of event kinds the user wants notifications for. If no preferences are set, the service uses defaults: text notes (1), follows (3), reactions (7), reposts (16), and long-form content (30023).

## Redis Keys

| Key Pattern | Type | Description |
|-------------|------|-------------|
| `user_tokens:{pubkey}` | Set | FCM tokens registered for a pubkey |
| `token_to_pubkey` | Hash | Reverse mapping from token to owner pubkey |
| `stale_tokens` | Sorted Set | Token timestamps for cleanup |
| `dedup:{event_id}` | String | Deduplication lock with TTL |
| `divine:preferences:{pubkey}` | String | JSON notification preferences |
