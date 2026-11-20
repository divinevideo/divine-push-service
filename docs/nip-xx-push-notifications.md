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
  "content": nip44_encrypt({"token": "<platform-token>", "timezoneOffsetMinutes": -300}),
  "sig": "<signature>"
}
```

The content field contains the NIP-44 encrypted token payload. Example plaintext structure:
```json
{ "token": "<platform-token>", "timezoneOffsetMinutes": -300 }
```

Optional campaign device fields inside the encrypted payload:

```json
{
  "token": "<platform-token>",
  "timezoneOffsetMinutes": -300,
  "campaignTargetTypes": ["app_route", "video_address", "creator_profile", "announcement", "featured_feed"],
  "platform": "android",
  "installChannel": "zapstore",
  "appVersion": "1.2.3"
}
```

`campaignTargetTypes` describes device navigation support independently of campaign
consent. Missing or malformed capabilities grant Following-only delivery. A valid
release metadata tuple is `ios`/`app_store`, `android`/`google_play`, or
`android`/`zapstore`, with a canonical `X.Y.Z` app version (components 0–999999,
no leading zeroes except `0`, no suffixes). These optional fields replace previous
metadata on each registration; incomplete or invalid metadata clears it and
suppresses release campaigns while preserving ordinary registration. Metadata
stays in the push service and is removed with the device registration.

**Note:** The exact payload structure is implementation-specific. Services define their own required fields.

**Rules:**
- `p` and `app` MUST be present. `expiration` MAY be present.
- `content` MUST be NIP-44 ciphertext; services MUST reject plaintext.
- This service does not use `expiration` to accept, reject, or remove a token.
- Timezone data is optional for registration, but this service sends no social,
  direct-message, or campaign push to a device that provided no valid
  `timezone` (IANA name) or `timezoneOffsetMinutes`. Such a token stays registered and
  receives nothing until the client registers again with timezone data.

#### Live Activity registration (iOS, optional)

An iOS app MAY also send a 3079 that starts, refreshes or ends an iOS Live
Activity for a post the person just published. The encrypted payload carries a
top-level `notificationsAuthorized` boolean and a `liveActivity` object beside
`token`:

```json
{
  "token": "<fcm-token>",
  "notificationsAuthorized": false,
  "liveActivity": {
    "activityPushToken": "<lowercase hex ActivityKit push token>",
    "postCoordinate": "34236:<author-pubkey-hex>:<d-tag>",
    "postEventIds": ["<64 hex event id>"],
    "postedAt": 1800000000,
    "windowEndsAt": 1800003600,
    "showFirstRunNote": false,
    "state": "active"
  }
}
```

Fields (all `liveActivity` fields are required):

| Field | Type | Meaning |
|-------|------|---------|
| `activityPushToken` | string | APNs Live Activity push token, 32 to 512 lowercase hex characters |
| `postCoordinate` | string | `<kind>:<author pubkey hex>:<d-tag>` of the post; kind is decimal digits, the pubkey is 64 lowercase hex, the `d` tag is non-empty |
| `postEventIds` | string array | 1 to 10 event ids (64 lowercase hex each); an edited post carries several |
| `postedAt` | integer | Unix seconds the post was published |
| `windowEndsAt` | integer | Unix seconds the updates stop |
| `showFirstRunNote` | boolean | Echoed back in every content state |
| `state` | string | `"active"` or `"ended"` |
| `notificationsAuthorized` (top level) | boolean | Whether the phone allows notifications |

**Rules:**
- A payload that carries either `liveActivity` (even as `null`) or
  `notificationsAuthorized` is a Live Activity registration. It never registers
  or refreshes the device for notifications, so a phone that declined
  notifications is never stored as a notification token.
- The author in `postCoordinate` MUST equal the event signer. An invalid object
  is dropped without a response.
- `windowEndsAt` MUST be after `postedAt` by at most 3600 seconds, and no more
  than an hour after the service's current time.
- Ending is a 3079 with `state: "ended"`, matched by `activityPushToken` and the
  signer's pubkey. It is never a 3080: a kind 3080 deregistration does not end
  an activity. An `ended` registration for an activity the service has never
  seen is remembered until `windowEndsAt` + 1800 seconds, so a later or replayed
  `active` for it never starts. Among events with the same `created_at`, an `ended`
  wins over an `active`.
- One active activity per signer and Firebase token: a newer one supersedes the
  previous one, which stops being updated. A signer holds a bounded number of
  active activities.
- The first signer to store an activity for an `activityPushToken` holds that
  token until the activity expires; another signer's `active` for it is refused.
- When the feature is off, an `active` registration is ignored; an `ended` one is
  still processed.
- The service pushes to the activity outside the notification caps, gap, quiet
  hours, presence checks and kind 3083 preferences; see
  [notification-delivery.md](notification-delivery.md#live-activity-updates).

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
- the token has had neither an accepted registration nor a successful delivery
  during the configured `token_max_age_days` inactivity window. Each accepted
  registration sets its activity time, and every delivered push refreshes it.

The control event's `expiration` timestamp does not determine how long the
stored token remains valid. The inactivity policy can therefore remove a token
that has received no registrations or pushes during the configured window,
even when the provider has not reported it invalid.

### Notification Preferences (kind 3083) — optional

Clients MAY send a preferences event to control which notification types they receive. Without this, the service sends all notification types it supports.

Preferences are keyed by the user's pubkey and persist when individual device tokens are deregistered.

```json
{
  "kind": 3083,
  "pubkey": "<client-pubkey>",
  "tags": [
    ["p", "<push-service-pubkey>"],
    ["app", "<app-id>"]
  ],
  "content": nip44_encrypt({"kinds": [1, 3, 7, 16], "campaignsEnabled": false}),
  "sig": "<signature>"
}
```

The decrypted content is a JSON object with a `kinds` array listing service-defined notification categories. Category values do not have to match trigger event kinds. In this service, category `1` controls Comment and Mention notifications triggered by supported kinds such as 1111, 30023, and 34236; the service does not subscribe to kind-1 text notes.
```json
{ "kinds": [1, 3, 7, 16], "campaignsEnabled": false }
```

An empty `kinds` array disables all notifications. Services SHOULD define sensible defaults for users who have not sent a preferences event.

`campaignsEnabled` is an independent, explicit opt-in for engagement campaign
pushes. Missing values read as `false`; the event-kind list never implies
campaign consent.

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

### Start and end a Live Activity (JS sketch)

```javascript
const now = Math.floor(Date.now() / 1000);
const liveActivity = {
  activityPushToken: activityTokenHex,        // lowercase hex from ActivityKit
  postCoordinate: `34236:${myPub}:${dTag}`,   // the author must be the signer
  postEventIds: [postEventId],
  postedAt: now,
  windowEndsAt: now + 3600,                    // at most one hour after postedAt
  showFirstRunNote: false,
  state: "active"
};

const event = {
  kind: 3079,
  pubkey: myPub,
  created_at: now,
  tags: [
    ["p", pushServicePubkey],
    ["app", "my-nostr-app"]
  ],
  content: await nip44.encrypt(
    pushServicePubkey,
    myPriv,
    JSON.stringify({ token: fcmToken, notificationsAuthorized: true, liveActivity })
  )
};
await relay.publish(await signEvent(event, myPriv));

// To end it, publish another 3079 with the same activityPushToken and
// state: "ended". A 3080 does not end a Live Activity.
```

### Update preferences

```javascript
const prefsPayload = { kinds: [1, 7, 16] }; // comments/mentions, likes, reposts

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
