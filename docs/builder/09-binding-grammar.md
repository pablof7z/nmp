# Live queries and the binding grammar

A live query starts with a Nostr filter, but set-valued fields may be driven by
closed bindings rather than fixed literals.

## The grammar

```text
Binding = Literal(set)
        | Reactive(CurrentPubkey)
        | Derived(inner: Demand, project: Selector)
        | SetOp(Union | Intersect | Diff, [Binding])

Selector = Authors | Ids | Tag(EventTagName) | AddressCoord

IndexedTagName = exactly one ASCII letter, A-Z or a-z
EventTagName   = an arbitrary Nostr tag-name string
```

Every node is serializable, hashable, comparable, printable, and available to
diagnostics. There are no app closures in the grammar.

The exact public constructors may change. The four-case algebra, explicit
context for every nested demand, and closed selector vocabulary are the
contract.

Filter tag keys and event tag names are different types. NIP-01 generic
`#<tag>` filters index exactly one case-sensitive ASCII letter; all 52 letters
are valid. Event tag names are arbitrary strings, so values such as `alt`, `-`,
or an application-defined name are valid event data and valid projection keys
even though they are not generic wire-filter keys. NMP has no whitelist of
blessed tag names.

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
let indexDemand = NMPDemand(
    selection: NMPFilter(
        kinds: .literal([appIndexKind]),
        authors: .reactive(.currentPubkey)
    ),
    source: .authorOutboxes,
    access: .public
)

let selectedRecords = NMPFilter(
    ids: .derived(
        inner: indexDemand,
        project: .tag("e")
    )
)
```

When the current user's index changes, NMP diffs the projected ids. It opens
demand for newly named records, withdraws records no longer named, preserves
shared demand, and updates the existing observation handle.

The app never owns the expanded id set or watches the inner kind to reopen
network subscriptions. The inner demand names its own source and access
context. It never implicitly inherits those values from the outer observation.
That prevents, for example, an inner user-list query from accidentally running
through an outer group host or borrowing its AUTH evidence.

`Derived` may feed any compatible set-valued field:

- projected authors into `authors`;
- projected ids or `e` tags into `ids`;
- a projected tag into another tag field; or
- address coordinates into address-aware selection.

For reference-bearing projections, the value does not travel alone. `e`, `a`,
and `p` tag selectors retain a valid relay hint from the tag; without one they
retain the relays where the source event was observed. `AddressCoord` retains
the source observation relays. This evidence is routed with the projected atom,
including through set operations, so an ids-only outer filter does not need an
unrelated global relay to rediscover where its target may live. Discovered
relay admission still applies; a network event cannot use a hint to bypass the
local/private-host policy.

When reasoning about or enforcing graph depth, count only `Derived` edges.
Nesting `SetOp` nodes combines values at the same level and consumes no
additional Derived hop.

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
        inner: NMPDemand(
            selection: NMPFilter(
                kinds: .literal([indexKind]),
                authors: .reactive(.currentPubkey)
            ),
            source: .authorOutboxes,
            access: .public
        ),
        project: .tag("e")
    )
}
```

Calling the helper produces the same public graph as writing it inline.
Diagnostics prints the expansion. The helper owns no subscription, cache, or
lifecycle.

NIP-02 may similarly expose `myFollows()` as a module-owned convenience that
expands to a kind:3 contact-list projection with its own user-list source and
access context. That does not make a follows-based feed or kind:1 a core
feature.

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

`group.sourceAuthority` is an opaque value scoped by the NIP-29 module to one
typed group/host context. The app may supply that protocol-defined public host;
it cannot turn the relay URL into generic authority for unrelated demand.

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
