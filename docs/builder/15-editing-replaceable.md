# Editing replaceable state safely

**Status: IMPLEMENTED CONTRACT.** NMP has an atomic exact-base guard for
whole-value edits. NIP-02 following (`nmp.follow(pubkey)` /
`nmp.unfollow(pubkey)`) is the first closed semantic operation, and the generic
guarded write is also available to Rust, Swift, and Kotlin callers that
legitimately own the complete event schema and their acquisition policy. The
contract is local and source-scoped; it never claims to know the globally newest
Nostr value.

## The destructive-write trap

Profiles, contact lists, relay lists, and parameterized replaceable events are
whole-value replacements. Reading an empty or stale cache and publishing only
the field an app meant to change can erase fields that exist elsewhere.

The canonical failure is a contact-list button that appends one `p` tag to an
unestablished local value. The new kind:3 wins by timestamp and silently
deletes every contact, relay hint, petname, content string, and unrelated tag
the app failed to copy.

An app-side read/modify/write helper cannot make that safe. The acquisition
evidence, protocol edit, canonical base check, signing, routing, and receipt
must be one NMP-owned operation.

## The implemented contract

### 1. Establish a source-scoped base

A protocol action opens an ordinary NMP live demand for the replaceable
coordinate. It receives the canonical local winner and the same
`AcquisitionEvidence` every other query receives.

A caller using the generic guarded payload instead must establish its own
source-scoped base through an ordinary live query and retain that exact row id.
The write guard does not manufacture acquisition evidence or turn a cache miss
into proof of remote absence.

NIP-02's closed default policy is ready only when every relay in the current
author-outbox plan is live, has reconciled the query shape, and reports no
shortfall. Cached rows render while acquisition continues, but cached-only,
AUTH-denied, source-error, local-limit, and no-planned-source states do not
authorize an edit.

If every current planned source reconciles and returns no kind:3, the resource
reports `NoContactList`. The ordinary `follow` and `unfollow` actions still
refuse to publish. First-list creation needs a separately named operation with
an explicit product policy; it cannot masquerade as editing an established
list. A bare cache miss is never treated as an empty list.

### 2. Compose from the exact value

The NIP-02 module owns kind:3 parsing and editing. It preserves the base
content and every unrelated tag byte-for-byte and in the same order.

- Follow appends one minimal valid `p` tag.
- Unfollow removes every matching `p` tag and nothing else.
- An already-satisfied relationship is a typed no-op and publishes nothing.

The app never reconstructs kind:3 and the UI never owns a second optimistic
follow Boolean.

### 3. Compare-and-swap at acceptance

The composed unsigned replacement carries the exact local base event id as an
acceptance precondition. The generic guard can express `None` for a future
protocol operation whose explicitly designed first-value policy permits it,
but NIP-02's ordinary follow action never emits that form.

Memory and redb stores check that precondition inside the same atomic
transaction that would allocate the intent/receipt and insert the pending
canonical row. If another winner arrived first, the store changes nothing and
the receipt emits:

```text
ReplaceableConflict { expected, actual }
```

The action does not silently rebuild on the new base. A caller may wait for the
live resource to refine and explicitly invoke a new action.

### 4. Use the ordinary write pipeline

After acceptance, the edit uses the normal durable write path: frozen author,
signer selection, canonical pending row, author-outbox routing, retry ownership,
and per-relay receipt facts. Dropping the button or action observer does not
cancel the durable obligation.

The compare-and-swap prevents a race against NMP's canonical local winner
before acceptance. It cannot prevent a previously unknown remote event from
appearing later; source evidence remains scoped and the newer valid winner will
still refine ordinary live queries.

## Generic native primitive

Use the generic payload only when the application owns the complete event
kind, tags, and content. In Swift:

```swift
let receipt = try await nmp.publish(
    WriteIntent(
        payload: .unsignedReplaceableEdit(
            pubkey: account,
            createdAt: timestamp,
            kind: 10_042,
            tags: completeTags,
            content: completeContent,
            expectedBase: observedRow?.id
        ),
        durability: .durable,
        routing: .authorOutbox
    )
)
```

The Kotlin shape is identical in meaning:

```kotlin
val receipt = nmp.publish(
    WriteIntent(
        payload = WritePayload.UnsignedReplaceableEdit(
            pubkey = account,
            createdAt = timestamp,
            kind = 10_042u,
            tags = completeTags,
            content = completeContent,
            expectedBase = observedRow?.id,
        ),
        durability = Durability.Durable,
        routing = WriteRouting.AuthorOutbox,
    ),
)
```

`expectedBase = nil` / `null` is not "I did not look." It asserts that the
new event's replaceable/addressable coordinate has no winner in NMP's local
canonical store at the instant of acceptance. A malformed id is refused at the
FFI boundary. A different winner at that exact coordinate produces
`replaceableConflict(expected, actual)` before durable receipt allocation,
journal mutation, signing, or pending-row insertion. The replacement still
passes the ordinary active-author check; the event's own kind and `d` tag
derive the coordinate, so an id observed at one coordinate cannot authorize a
write to another.

After acceptance there is no special pipeline: NMP owns signer selection,
canonical pending state, routing, retry, reattachment, and receipt evidence.
For a protocol whose preservation and readiness rules should be reusable across
apps, prefer a closed semantic operation such as NIP-02 following.

## Swift semantic API

Use the action directly when an application owns its own presentation:

```swift
let action = nmp.follow(targetPubkey)

for await status in action.status {
    switch status {
    case .acquiring:
        break
    case .noChange:
        break
    case .receipt(_, let writeStatus):
        render(writeStatus)
    case .failed(let reason):
        render(reason)
    }
}
```

Every operational outcome is stream state, including malformed target,
signed-out account, source failure, acquisition timeout, no-op, atomic base
conflict, signing, routing, and relay results. `follow` itself does not throw.

For a bindable live relationship:

```swift
let following = try NMPFollowing(engine: nmp, target: targetPubkey)

NMPFollowButton(following: following)
NMPUserCard(pubkey: targetPubkey, profile: profile, following: following)
```

`NMPFollowing` copies NMP's relationship, availability, and action streams onto
the main actor. `NMPFollowButton` renders that state and forwards a tap to the
resource. Neither type parses tags, chooses a base, opens a second cache,
selects relays, signs, retries, or invents success.

## Extending the pattern

`UnsignedReplaceableEdit` is the generic Rust write payload, mirrored as
`FfiWritePayload::UnsignedReplaceableEdit`,
Swift `.unsignedReplaceableEdit`, and Kotlin
`WritePayload.UnsignedReplaceableEdit`. It is the low-level exact-base
acceptance primitive for protocol modules and applications that own the full
event value. It deliberately owns no kind-specific parsing, merge, or source
readiness policy.

Another replaceable protocol helper must still define and falsify:

- its source authority and readiness policy;
- exact preservation rules for fields it does not own;
- first-value policy when the source-scoped base is `None`;
- conflict and retry UX;
- access-context isolation for private or AUTH-scoped state.

The existence of the generic guard does not bless arbitrary app-authored
read/modify/write helpers.

## Proof

The shipped falsifiers cover:

- tag order, content, relay hint, petname, duplicate-target, and unrelated-tag
  preservation;
- exact-base success, generic `None`-means-`None`, regular-event misuse
  rejection, and a concurrent winner producing a typed conflict with no
  journal residue;
- FFI, Swift, and Kotlin arbitrary-kind exact-base, stale-base, and first-value
  paths, including a native store-acceptance test rather than conversion-only
  coverage;
- signed-out, no-source, and reconciled-no-contact-list failure without a
  write;
- a real loopback indexer/outbox relay through both direct Rust and the iOS FFI
  surface: initial state, follow/ACK, reactive following state, duplicate
  follow no-op, preservation of an existing contact, unfollow/ACK, and reactive
  not-following state;
- Swift action-state mapping and Gallery accessibility/runtime behavior.

---

<!-- nav-footer -->
<sub>← [Writing: accepted intent, local state, and relay evidence](14-writing.md) · [Index](README.md) · [Identity](16-identity.md) →</sub>
