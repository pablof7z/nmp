# Current pubkey, accounts, and signer selection

NMP consumes identity inputs and signer capabilities. It does not own an
account manager.

## The app owns accounts

Your app owns:

- which identities exist;
- labels, ordering, and account-selection UI;
- import, export, backup, and removal policy;
- whether an identity is local, remote, hardware-backed, or disposable; and
- whether logout should preserve or destroy the shared local cache.

NMP needs a current-pubkey value for reactive demand and a way to locate a
signer when a write needs one.

## Current pubkey has two ergonomic roles

```swift
try engine.setCurrentPubkey(selectedAccount.pubkey)
```

1. `Reactive(CurrentPubkey)` bindings re-root to the new value.
2. A write with no signer override selects the registered signer for that value.

Those roles share a default but remain separable.

Changing current pubkey does not:

- rewrite literal multi-account demands;
- isolate or clear cached rows;
- retarget already accepted writes;
- require a signer to exist for read-only use; or
- make every operation use the new identity.

## Read-only and multi-account demand

A current pubkey may have no signer. Read-only browsing remains valid.

An app that watches all of its accounts writes the literal demand it actually
wants:

```swift
let mentions = NMPFilter(
    kinds: .literal([appKind]),
    tags: ["p": .literal(allAccountPubkeys)]
)
```

That query stays unchanged when the selected/current account changes. App state
can annotate each row with which local account was tagged.

## Register capabilities, not signer objects on every call

Ordinary writes should not force the app to pass a signer repeatedly:

```swift
try engine.attachSigner(keychainProvider, for: accountPubkey)
let receipt = try engine.publish(.init(
    draft: draft,
    durability: .durable
))
```

The provider may be local, NIP-46, hardware-backed, or app-defined. NMP asks it
to sign one exact frozen body when needed.

Platform SDKs should ship standard providers backed by Keychain or Android
Keystore so every app does not hand-roll secure-storage plumbing. Rust persists
signing obligations and identity references, not raw secrets. The app can still
supply a custom provider.

## Override one write without changing current pubkey

```swift
let receipt = try engine.publish(.init(
    draft: episodeDraft,
    durability: .durable,
    signer: .identity(podcastIdentity)
))
```

This supports podcast keys, disposable identities, delegates, hardware keys,
and remote signers. It does not alter reactive queries rooted at current pubkey.

NMP resolves and pins the chosen identity at acceptance. A later account switch
cannot redirect the pending intent.

## Provider absence and reattachment

Once NMP can resolve the expected author identity, absence of a matching signer
does not block durable acceptance into the canonical store and receipt journal:

```text
accepted(intentId)
awaitingSigner(pubkey)
```

The unsigned pending row remains visible to matching queries. Attaching a
matching provider later resumes the existing obligation:

```swift
try engine.attachSigner(reconnectedBunker, for: pubkey)
```

The app does not recreate the intent or mutate the pending row. A provider
disconnect is capability state, not permission to discard accepted data.

## Shared cache trust domain

One engine instance has one canonical cache. Accounts in that engine are not
separate mutually untrusted users. Validated public rows and locally accepted
rows remain available to any local query that matches them.

For a device/app used by mutually untrusted people, logout must be explicit:

```swift
try await engine.reset(.allLocalData)
```

The destructive operation atomically clears cached events, pending writes,
receipts, source/access evidence, protocol state, and attached capability
references according to the reset contract. An ordinary current-pubkey change
must never pretend to provide that boundary.

## AUTH identity is query context

Relay AUTH may change what a source returns. A demand therefore carries access
context independently of the app's selected account:

```swift
let demand = NMPDemand(
    selection: selection,
    source: group.sourceAuthority,
    access: .auth(identityRef)
)
```

The protocol module mints `group.sourceAuthority` from validated group state;
the app cannot grant protocol-host authority to an arbitrary relay URL.

Evidence from one AUTH identity cannot prove acquisition for another. The app
still owns whether and when that identity is acceptable for product policy.

---

<sub>[Index](README.md) · Related: [Writing and receipts](14-writing.md) · [Source and routing context](17-relays.md)</sub>
