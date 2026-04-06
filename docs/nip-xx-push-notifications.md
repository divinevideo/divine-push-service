NIP-XX
======

Push Notifications
------------------

`draft` `optional`

Define a standard for registering push tokens and receiving notifications when clients aren't connected to relays.

## Abstract

Clients register encrypted push tokens with a push service. Services watch relays and deliver notifications to registered devices.

## Motivation

Avoid always-on connections (battery), deliver timely alerts, and enable secure token management through encryption.

## Specification

### Event Kinds

- `3079`: Push token registration
- `3080`: Push token deregistration
- `3083`: Notification preferences (optional)

All event content fields MUST contain NIP-44 ciphertext strings. The decrypted payload structure is defined below.

### Registration (kind 3079)

```json
{
  "kind": 3079,
  "pubkey": "<client-pubkey>",
  "tags": [
    ["p", "<push-service-pubkey>"],
    ["app", "<app-id>"],
    ["expiration", "<unix-seconds>"]
  ],
  "content": nip44_encrypt({"token": "<platform-token>"}),
  "sig": "<signature>"
}
```

The content field contains the NIP-44 encrypted token payload. Example plaintext structure:
```json
{ "token": "<platform-token>" }
```

**Note:** The exact payload structure is implementation-specific. Services define their own required fields.

**Rules:**
- `p`, `app`, `expiration` MUST be present.
- `content` MUST be NIP-44 ciphertext; services MUST reject plaintext.
- Expiration per NIP-40. Servers MUST ignore expired events. Clients SHOULD refresh early (30–90d).

### Deregistration (kind 3080)

Same structure and rules as 3079.

```json
{
  "kind": 3080,
  "pubkey": "<client-pubkey>",
  "tags": [
    ["p", "<push-service-pubkey>"],
    ["app", "<app-id>"],
    ["expiration", "<unix-seconds>"]
  ],
  "content": nip44_encrypt({"token": "<platform-token>"}),
  "sig": "<signature>"
}
```

### Notification Preferences (kind 3083) — optional

Clients MAY send a preferences event to control which notification types they receive. Without this, the service sends all notification types it supports.

```json
{
  "kind": 3083,
  "pubkey": "<client-pubkey>",
  "tags": [
    ["p", "<push-service-pubkey>"],
    ["app", "<app-id>"]
  ],
  "content": nip44_encrypt({"kinds": [1, 3, 7, 16]}),
  "sig": "<signature>"
}
```

The decrypted content is a JSON object with a `kinds` array listing the event kinds the user wants notifications for:
```json
{ "kinds": [1, 3, 7, 16] }
```

An empty `kinds` array disables all notifications. Services SHOULD define sensible defaults for users who have not sent a preferences event.

## Notification Triggers

Services define which events trigger notifications. A typical single-app service watches for specific event kinds (reactions, replies, follows, mentions, reposts) and notifies users who are tagged or referenced.

Services MAY support additional trigger logic beyond kind matching.

## Implementation Requirements

### Push Service

1. **Encryption**: Reject plaintext for all event kinds. Content must be valid NIP-44 ciphertext.
2. **App isolation**: Partition by app tag; ignore events with unknown app.
3. **Expiration**: Ignore expired events (NIP-40).
4. **Multiple devices**: Support multiple tokens per (pubkey, app).
5. **Idempotency**: At most one notification per (recipient_pubkey, app, event_id).
6. **Error handling**: Remove invalid tokens on provider errors.
7. **Token security**: Protect stored tokens; redact in logs.
8. **Targeting**: If `p` tag is present and not this service's pubkey, ignore.

### Client

1. Encrypt with NIP-44 to service pubkey.
2. Follow service's documentation for required payload fields.
3. Stable app id per application.
4. Refresh before expiration; deregister on logout.
5. Verify service identity via discovery.

## Security

- **Token privacy**: Publishing {pubkey ↔ token} enables correlation; NIP-44 mitigates.
- **Replay**: Expiration (NIP-40) bounds replays.
- **Rotation**: Refresh/rotate tokens to limit exposure.
- **Isolation**: `app` tag prevents cross-app misuse.

## Examples

Examples use `nip44_encrypt(...)` as pseudocode; actual content MUST be the ciphertext string.

### Register (JS sketch)

```javascript
const tokenPayload = { token: fcmToken };

const event = {
  kind: 3079,
  pubkey: myPub,
  created_at: Math.floor(Date.now() / 1000),
  tags: [
    ["p", pushServicePubkey],
    ["app", "my-nostr-app"],
    ["expiration", String(Math.floor(Date.now() / 1000) + 7776000)]
  ],
  content: await nip44.encrypt(
    pushServicePubkey,
    myPriv,
    JSON.stringify(tokenPayload)
  )
};
await relay.publish(await signEvent(event, myPriv));
```

### Update preferences

```javascript
const prefsPayload = { kinds: [1, 7, 16] }; // only replies, likes, reposts

const event = {
  kind: 3083,
  pubkey: myPub,
  created_at: Math.floor(Date.now() / 1000),
  tags: [
    ["p", pushServicePubkey],
    ["app", "my-nostr-app"]
  ],
  content: await nip44.encrypt(
    pushServicePubkey,
    myPriv,
    JSON.stringify(prefsPayload)
  )
};
await relay.publish(await signEvent(event, myPriv));
```

### Service Discovery

Services can advertise their availability through various means. See [NIP-89](https://github.com/nostr-protocol/nips/blob/master/89.md) for application handler discovery patterns.
