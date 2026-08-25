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

A service MAY additionally read a public NIP-51 list to support per-author
subscriptions; see [Author subscriptions](#author-subscriptions-kind-30000).
That list is deliberately **not** encrypted, because the service must be able to
read it. It is not one of the control kinds above and carries no ciphertext.

### Registration (kind 3079)

The `expiration` tag in this example is optional. Clients may include it for
NIP-40 relay interoperability, but this service does not interpret it.

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
- `p` and `app` MUST be present. `expiration` MAY be present.
- `content` MUST be NIP-44 ciphertext; services MUST reject plaintext.
- This service does not use `expiration` to accept, reject, or remove a token.

### Deregistration (kind 3080)

Uses the same encrypted token payload and required `p` and `app` tags as 3079.
It MAY also carry an `expiration` tag under the same optional policy.

```json
{
  "kind": 3080,
  "pubkey": "<client-pubkey>",
  "tags": [
    ["p", "<push-service-pubkey>"],
    ["app", "<app-id>"]
  ],
  "content": nip44_encrypt({"token": "<platform-token>"}),
  "sig": "<signature>"
}
```

### Token lifecycle

The service removes a stored registration under any of these conditions:

- the client publishes an explicit kind 3080 deregistration;
- the push provider reports that the token is no longer registered or valid; or
- the token has not had a successful delivery for the configured
  `token_max_age_days` inactivity window. Registration sets the initial
  activity time, and every delivered push refreshes it.

The control event's `expiration` timestamp does not determine how long the
stored token remains valid. The inactivity policy can therefore remove a token
that has received no pushes during the configured window, even when the
provider has not reported it invalid.

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

### Author subscriptions (kind 30000)

Every trigger described so far is *someone acted on your content*, resolved from
`p` tags on the trigger event. A service MAY also support the inverse — notify a
user when an author they subscribed to publishes — by reading a NIP-51 people
list with a reserved `d` identifier:

```json
{
  "kind": 30000,
  "tags": [
    ["d", "notify"],
    ["title", "Notify"],
    ["p", "<author-pubkey-hex>"],
    ["p", "<author-pubkey-hex>"]
  ],
  "content": ""
}
```

Requirements:

- The list is a parameterised replaceable event: the tuple (pubkey, `30000`,
  `notify`) identifies it, and each publish REPLACES the previous set. Clients
  MUST publish the complete list on every change, never a delta.
- `content` MUST be empty. Encrypting it would make the list unreadable to the
  service and defeat the mechanism.
- `p` tags MUST carry full-length hex pubkeys.
- An empty `p` set is **valid** and means "no subscriptions". Services MUST treat
  it as a clearing update, not as a malformed event.
- Services MUST ignore kind 30000 events whose `d` tag is not exactly `notify`.
- Services MUST NOT age these events out on a replay horizon. A list published
  long ago and never edited is still current.
- Services SHOULD guard against out-of-order delivery by tracking the last
  applied list and rejecting anything older; relays may deliver a stale
  replacement after a newer one.
- Services MUST resolve a `created_at` tie the way NIP-01 resolves it, by
  retaining the event with the lowest id. Resolving by arrival order instead
  makes the service's state diverge permanently from the relay's, so a rebuild
  from relay history answers differently than what was served live. Tracking the
  last applied `created_at` alone is not enough for this; the id has to be kept
  alongside it.

Because the list is public, **who a user subscribes to is public**. Clients
SHOULD surface that rather than implying the subscription is private.

Notifications generated this way SHOULD be rate-limited per (subscriber, author);
a prolific author otherwise trains users into disabling notifications entirely.

## Notification Triggers

Services define which events trigger notifications. A typical single-app service watches for specific event kinds (reactions, replies, follows, mentions, reposts) and notifies users who are tagged or referenced.

Services MAY support additional trigger logic beyond kind matching, including
recipient sets that come from subscriber-published state rather than from tags
on the trigger event (see [Author subscriptions](#author-subscriptions-kind-30000)).

## Implementation Requirements

### Push Service

1. **Encryption**: Reject plaintext for all event kinds. Content must be valid NIP-44 ciphertext.
2. **App isolation**: Partition by app tag; ignore events with unknown app.
3. **Control-event age**: Ignore control events whose `created_at` is beyond the seven-day replay horizon.
4. **Multiple devices**: Support multiple tokens per (pubkey, app).
5. **Idempotency**: At most one notification per (recipient_pubkey, app, event_id).
6. **Error handling**: Remove invalid tokens on provider errors.
7. **Token security**: Protect stored tokens; redact in logs.
8. **Targeting**: If `p` tag is present and not this service's pubkey, ignore.

### Client

1. Encrypt with NIP-44 to service pubkey.
2. Follow service's documentation for required payload fields.
3. Stable app id per application.
4. Register when the push session becomes ready, republish when the provider token rotates, and deregister on logout.
5. Verify service identity via discovery.

## Security

- **Token privacy**: Publishing {pubkey ↔ token} enables correlation; NIP-44 mitigates.
- **Replay**: Control events older than the seven-day replay horizon are ignored, and repeat processing of the same event ID is suppressed for `processed_event_ttl_secs`.
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
