# Kind-diverse example sketches

These sketches test whether the two nouns remain useful across unrelated Nostr
shapes. They are design probes, not core recipes.

## 1. App-owned records

Observe one caller-owned kind from literal authors:

```text
selection: kinds = Literal([appKind])
           authors = Literal(selectedAuthors)
source:    Auto
access:    Public
```

This is the neutral primitive quickstart. NMP knows no content semantics.

## 2. App-owned derived index

An app-owned index event names record ids in `e` tags:

```text
outer.ids = Derived(
  inner: Demand {
    selection: kinds:[appIndexKind], authors:[CurrentPubkey],
    access: Public
  },
  project: Tag(e)
)
```

The engine owns the expanded ids and incremental demand repair. The inner
index demand carries its own source/identity rather than inheriting the
outer observation's context.

## 3. Multi-account addressed data

```text
kinds:[appKind], #p:Literal(allLocalAccountPubkeys)
```

This literal query stays live when current pubkey changes. The app annotates
which account each row addresses.

## 4. NIP-02 reusable authors

```swift
let authors = Nip02.myFollows()
let selection = NMPFilter(
    kinds: .literal(callerSelectedKinds),
    authors: authors
)
```

The NIP-02 module owns the kind:3 projection. The caller chooses outer kinds
and presentation. Core owns neither a home feed nor kind:1 preference.

## 5. NIP-29 group management

```swift
let list = parseSimpleGroupsListTolerant(row)          // NIP-51 Simple groups via NIP-29 capability
let selected = list.items[0]                           // app-owned selection
let scope = try NMPRelayScope.on([selected.hostRelay]) // named once -- NIP-29
let group = scope.group(selected.groupID)
let receipt = try group.addUser(
    engine: engine, authorPubkeyHex: myPubkeyHex, pubkeyHex: pubkey, role: "admin"
)
```

The remembered group/host list is NIP-51 kind `10009`. `nmp-nip29` exposes its
observational decode as part of NMP's remembered-groups product capability;
NIP-29 owns its group metadata, membership, role, moderation schemas,
relay-scope authority, and semantic operations. The decode does not authorize a
host or route, and no second projection wraps it. A group can live on more than
one relay; the app names the relay set once and never passes a host to an
individual operation. The app selects one entry and hands the exact fields a
NIP-29 operation needs.

## 6. NIP-C7 chat message in a NIP-29 group

```swift
let message = NipC7.chat(text)
let receipt = try group.publish(message, using: engine)
```

Two owners compose without a content monopoly: NIP-C7 owns the kind:9 schema
and NIP-29 adds only group context. Core signs once and routes one intent.

## 7. Podcast identity override

```swift
let receipt = try engine.publish(.init(
    draft: episodeDraft,
    durability: .durable,
    signer: .identity(podcastIdentity)
))
```

The podcast key signs this event without becoming current pubkey. If its
signing provider is offline, the canonical pending row remains visible and the receipt
waits for the configured provider to become available.

## 8. Explicitly non-durable presence signal

```swift
let receipt = try engine.publish(.init(
    draft: presenceDraft,
    durability: .nonDurable
))
```

The publication obligation is not resumed after process loss. Receipt facts are
still observable and reattachable; process loss becomes an explicit policy-
abandoned terminal rather than silent disappearance.

## 9. AUTH-scoped protocol query

```swift
let demand = group.demand(
    selection: groupOwnedSelection,
    accessIdentity: groupIdentity
)
```

The module mints validated host authority. Evidence remains scoped to the AUTH
identity and cannot prove acquisition for another context.

## 10. Permanent diagnostics

Every example above should be explainable on screen: descriptor expansion,
read routing, exact relay filters, access/AUTH, received kinds, pending
intent, signer state, route reasons, attempts, caps, and shortfall.

If an example needs app-owned subscription repair, raw relay expansion, a
second cache, or a hidden module lifecycle, it has falsified the architecture.

---

<sub>[Index](README.md) · Related: [Ten-minute embedding](04-ten-minute-timeline.md) · [Using protocol modules](27-recipes-and-choosing.md) · [Diagnostics](22-diagnostics.md)</sub>
