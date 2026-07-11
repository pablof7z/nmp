# Identity: reactive reads, default signing, and explicit overrides

**Status: CURRENT + TARGET.** `addAccount` / `setActiveAccount` and
`Reactive(ActivePubkey)` are built. The current implementation also moves one
active signer with that pubkey. The target contract below keeps the ergonomic
default but removes that coupling as an authority boundary: a write may select
another registered signer without changing reactive reads, and an accepted
write never changes signer because the current pubkey later changes.

## Three independent questions

Identity appears in three places, and NMP must not collapse them:

1. **Which value does a reactive query use?** `Reactive(ActivePubkey)` reads the
   engine's current-pubkey input.
2. **Which capability signs this write?** By default, the signer registered for
   the current pubkey. A write can explicitly override that identity.
3. **Which identity authenticates or decrypts?** AUTH and crypto capabilities
   are selected for their operation; neither silently changes query inputs or
   the signer of another write.

Most apps use the default for all three. Keeping them independently expressible
is what supports podcast keys, disposable identities, hardware signers, NIP-46,
and multi-account views without forcing the app to pretend one key is globally
active for every purpose.

## The normal path stays small

```swift
let alice = try await nmp.addAccount(secretKey: "nsec1...")
try nmp.setCurrentPubkey(alice)       // target name; current API is setActiveAccount

let receipt = try await nmp.publish(draft)  // defaults to Alice's registered signer
```

The app does not pass or retain a signer object on every publish. NMP keeps a
registry of signer capabilities keyed by stable identity. The current pubkey is
the default selection because that is correct for the common case.

The exceptional path is explicit:

```swift
let receipt = try await nmp.publish(draft, as: podcastIdentity)
```

That override selects a registered capability; it does not expose key material
to the call site and does not change `$currentPubkey`.

## Reactive queries re-root by dependency

Changing the current pubkey re-resolves only descriptors that depend on
`Reactive(ActivePubkey)`. For example:

```swift
NMPFilter(kinds: [9999], authors: .reactive(.activePubkey))
```

Switching A to B makes that same live query withdraw the no-longer-needed A
demand and open the B demand. Unchanged graph nodes remain shared.

A literal multi-account query does not depend on the current pubkey and remains
unchanged:

```swift
NMPFilter(kinds: [9999], tags: ["p": .literal([accountA, accountB])])
```

There is no global account-switch barrier and no rule that all demand belonging
to another registered identity must disappear. Teardown is dependency-driven:
only demand no longer referenced by any live descriptor is withdrawn.

## Accepted writes pin identity

Signer selection happens before durable acceptance becomes visible:

- no override -> capture the identity associated with the current pubkey;
- override -> capture the requested identity;
- already signed draft -> verify and preserve its author/signature.

Once accepted, the write is permanently associated with that identity. Later
changes to the current pubkey cannot redirect signing, retries, receipts, or
AUTH to a different key.

If the selected signer is unavailable, the durable intent remains
`AwaitingSigner(pubkey)`. Attaching a valid capability for that pubkey resumes
it. The app may cancel it; NMP does not silently reassign or discard it.

## One engine is one local trust domain

Registered accounts share the engine's public event cache. A public event that
matches several queries is one canonical row regardless of which identity led
to its acquisition. AUTH connection state and per-source evidence may still be
keyed by access context because relays can expose different results after AUTH;
that is acquisition correctness, not a claim that accounts distrust each other.

An app serving mutually untrusted people must use the explicit destructive
reset/logout operation to clear cached events, pending writes, receipts,
coverage/evidence, and retained capabilities before handing the app over.
Changing the current pubkey is not a privacy wipe.

## What the app still owns

NMP does not own an account list, login flow, account switcher, avatar, or
"primary account" policy. The app owns those. NMP owns registered capability
references, reactive dependency resolution, and already-accepted obligations.

## Current implementation gap

The shipping `setActiveAccount` verb still re-roots reads and moves the active
signer together, and the SDK has no per-write signer override or destructive
reset. Treat that as current implementation truth, not the target invariant.
The public shape is provisional; the behavior above is the contract the next
work must falsify across Rust, FFI, Swift, and Kotlin.

---

<!-- nav-footer -->
<sub>← [Editing replaceable state safely](15-editing-replaceable.md) · [Index](README.md) · [Relays: outbox & indexers](17-relays.md) →</sub>
