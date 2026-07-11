# Live queries and the binding grammar

A live query starts with a Nostr filter, but set-valued fields may be driven by
closed bindings rather than fixed literals.

## The grammar

```text
Binding = Literal(set)
        | Reactive(CurrentPubkey)
        | Derived(inner: Filter, project: Selector)
        | SetOp(Union | Intersect | Diff, [Binding])

Selector = Authors | Ids | Tag(char) | AddressCoord
```

Every node is serializable, hashable, comparable, printable, and available to
diagnostics. There are no app closures in the grammar.

The exact public constructors may change. The four-case algebra and closed
selector vocabulary are the contract.

## Literal: fixed app demand

```swift
let selection = NMPFilter(
    kinds: .literal([appKind]),
    authors: .literal(selectedAuthors),
    tags: ["d": .literal(selectedRecordIds)]
)
```

A literal is still live because the local store and planned sources continue to
change. "Reactive" here means the binding's input changes without replacing the
descriptor.

## Reactive: current pubkey as one input

```swift
let addressedToCurrentUser = NMPFilter(
    kinds: .literal([appKind]),
    tags: ["p": .reactive(.currentPubkey)]
)
```

Changing current pubkey re-roots this demand. It does not change a simultaneous
literal query containing all account pubkeys, clear the shared cache, or retarget
an already accepted write.

`CurrentPubkey` is the only initially promoted generic reactive root. Apps can
already rebuild a descriptor from their own state when other inputs change.
Adding arbitrary named reactive roots is a governed design decision, not a
hidden callback registry.

## Derived: let stored Nostr state drive another field

Suppose an app-owned index event contains `e` tags naming the records the app
should observe. The app declares the relationship once:

```swift
let selectedRecords = NMPFilter(
    ids: .derived(
        inner: NMPFilter(
            kinds: .literal([appIndexKind]),
            authors: .reactive(.currentPubkey)
        ),
        project: .tag("e")
    )
)
```

When the current user's index changes, NMP diffs the projected ids. It opens
demand for newly named records, withdraws records no longer named, preserves
shared demand, and updates the existing observation handle.

The app never owns the expanded id set or watches the inner kind to reopen
network subscriptions.

`Derived` may feed any compatible set-valued field:

- projected authors into `authors`;
- projected ids or `e` tags into `ids`;
- a projected tag into another tag field; or
- address coordinates into address-aware selection.

## Set operations: compose closed sets

```swift
let visibleAuthors: NMPBinding = .setOp(
    .diff,
    [allowedAuthors, blockedAuthors]
)

let selection = NMPFilter(
    kinds: .literal([appKind]),
    authors: visibleAuthors
)
```

`Union`, `Intersect`, and `Diff` operate on resolved sets. The engine can explain
which child contributed or withdrew each value and can recompile only affected
wire demand.

## Reusable fragments are ordinary values

An app or protocol module may package a constructor:

```swift
func recordsNamedByCurrentUser(
    indexKind: UInt16
) -> NMPBinding<EventId> {
    .derived(
        inner: NMPFilter(
            kinds: .literal([indexKind]),
            authors: .reactive(.currentPubkey)
        ),
        project: .tag("e")
    )
}
```

Calling the helper produces the same public graph as writing it inline.
Diagnostics prints the expansion. The helper owns no subscription, cache, or
lifecycle.

NIP-02 may similarly expose `myFollows()` as a module-owned convenience that
expands to a kind:3 contact-list projection. That does not make a follows-based
feed or kind:1 a core feature.

## Selection is not the whole descriptor

The same selection can be acquired under different source or access authority:

```swift
let publicDemand = NMPDemand(
    selection: selection,
    source: .authorOutboxes,
    access: .public
)

let authenticatedDemand = NMPDemand(
    selection: selection,
    source: group.sourceAuthority,
    access: .auth(groupIdentity)
)
```

`group.sourceAuthority` is an opaque, validated value minted by the NIP-29
module. The app cannot manufacture protocol-host authority from an arbitrary
relay URL.

NMP may share local selection matching. It may share wire work and acquisition
evidence only where the full context makes sharing valid.

## Why selectors are closed

An app closure such as `project: { rows in ... }` would prevent stable hashing,
incremental dependency tracking, cross-platform parity, and diagnostics. When a
real protocol cannot be expressed by the selector vocabulary, the correct next
step is a public grammar proposal with defined identity, routing, persistence,
printing, and platform projection.

It is not an escape hatch.

---

<sub>[Index](README.md) · Related: [Ownership reference](05-two-nouns.md) · [Snapshots and evidence](10-consuming-results.md)</sub>
