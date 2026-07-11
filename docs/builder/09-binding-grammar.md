# Live queries & the binding grammar

**Status: CURRENT + TARGET.** The four-case binding grammar is built. The
current-pubkey binding is the only built reactive input; broader reusable
declarations and the query's source/access context remain provisional.

After this chapter you can express derived demand as one closed value and let
the engine keep it live as store inputs and the current pubkey change. No
content kind or feed composition is privileged by the grammar.

## The read noun is a filter whose fields are bindings

A live query is a Nostr `Filter` — `kinds`, `authors`, `ids`, `#`-tags, `since/until/limit` — except that each *field value* is not a literal set but a `Binding`. A binding is one of exactly four things:

```
Binding = Literal(set)                     // a fixed hex/tag-value set
        | Reactive(ActivePubkey)           // re-resolves when current pubkey changes
        | Derived(inner: Filter, project: Selector)   // rows of an inner query, projected to a value set
        | SetOp(op, [Binding])             // union / intersect / diff over child bindings
```

That is the entire grammar. It is a *value*: hashable, serializable, comparable, identical on every platform. In Swift it is `NMPBinding`; in Rust it is `nmp_grammar::Binding`. There is no closure, no callback, no app code anywhere inside it — a point this chapter's last section makes load-bearing.

```swift
// Swift — Packages/NMP/Sources/NMP/NMPFilter.swift
public indirect enum NMPBinding: Sendable, Hashable {
    case literal(Set<String>)
    case reactive(NMPIdentityField)                        // .activePubkey
    case derived(inner: NMPFilter, project: NMPSelector)
    case setOp(NMPSetAlgebra, [NMPBinding])                // .union / .intersect / .diff
}
```

Because a filter is a value, "editing a running query" does not exist and is not needed: you construct a *new* `NMPFilter` and observe it. Dropping the old handle withdraws its demand; the engine shares graph nodes between any two identical descriptors, so re-observing something already live is free.

## Example 1 — a reusable NIP-02-derived author set

This example proves generic derivation; it does not define a canonical NMP feed.
A NIP-02 module or app package may expose the derived binding commonly called
`myFollows`, provided it returns this closed expansion rather than hiding app
code inside the graph.

```swift
// apps/Falsifier/Sources/Falsifier/FeedFilters.swift
static func follows(kinds: [UInt16]) -> NMPFilter {
    NMPFilter(
        kinds: kinds, // the caller chooses any content kinds
        authors: .derived(
            inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
            project: .tag("p")
        ),
        limit: 200
    )
}
```

Read it inside-out. The inner filter is `kind:3` events by
`Reactive(ActivePubkey)` — the current pubkey's contact list. `project:
.tag("p")` selects its `p`-tag values. The outer filter asks for whatever event
kinds the caller supplied, authored by that projected set.

The same value in Rust:

```rust
// nmp-resolver/tests/contract.rs — my_follows_filter()
Filter {
    kinds: Some(BTreeSet::from([1u16])),
    authors: Some(Binding::Derived(Box::new(Derived {
        inner: Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        project: Selector::Tag(TagName::new('p').unwrap()),
    }))),
    ..Filter::default()
}
```

When the contact list drops C and adds D, the demand delta withdraws C's
no-longer-referenced content demand and opens D's. Unchanged authors and any
other live descriptor still referencing C remain untouched.

## Example 2 — protocol-aware group discovery (illustrative)

The raw grammar can express nested dependencies, but a production NIP-29
module should expose typed group references that retain host-relay and protocol
meaning. This snippet demonstrates derivation only; it is not the final module API.

```swift
// NIP-29 groups-I'm-in, in the same NMPFilter algebra
NMPFilter(
    kinds: [39000, 39001, 39002],
    tags: [
        "d": .derived(
            inner: NMPFilter(
                kinds: [39002],
                tags: ["p": .reactive(.activePubkey)]
            ),
            project: .tag("d")
        )
    ]
)
```

Two things worth noticing. First, `Reactive(ActivePubkey)` sits in a *tag* field (`#p`) here, not in `authors` — the grammar is position-agnostic, and the resolver never branches on which field a binding occupies. Second, this is depth-2: a `Derived` binding whose `inner` filter is itself reactive. When a new membership event adds you to group `g3`, the cascade is exactly one level deep — the resolver opens a single new outer atom (`#d = g3`) and touches nothing else. The inner membership atom is never re-opened; the graph's node structure is untouched. Depth buys you correctness, not churn.

## Example 3 — follows minus mutes (`SetOp` / `Diff`)

The compound every real client needs: notes from people I follow *but have not muted*. Without set algebra you would be forced to fetch both sets into the app and subtract them yourself — reviving exactly the "app owns interest expansion" bug the grammar exists to kill (bug-ledger #11). `SetOp(Diff, …)` makes it declarable:

```swift
let follows: NMPBinding = .derived(
    inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
    project: .tag("p")
)
let mutes: NMPBinding = .derived(
    inner: NMPFilter(kinds: [10000], authors: .reactive(.activePubkey)),
    project: .tag("p")
)
let selection = NMPFilter(
    kinds: [9999], // arbitrary caller-selected outer kind
    authors: .setOp(.diff, [follows, mutes])       // follows − mutes
)
```

`Diff` is "first operand minus the union of the rest"; `Union` and `Intersect`
round out the algebra. When one projected pubkey leaves the result, only its
no-longer-referenced outer demand closes. The engine does not branch on the
outer kind.

## The closed-Selector rule: values in, code after

Look again at the one part of a `Derived` binding you might be tempted to make programmable — `project`. It is a **closed vocabulary**, `Selector`, with exactly four members:

```
Selector = Authors | Ids | Tag(name) | AddressCoord
```

`Authors` projects each matched event's author; `Ids` its id; `Tag(name)` a single-letter tag's values; `AddressCoord` the `(kind, author, d)` coordinate (which fans out into one co-pinned atom per coordinate). That is all a projection can be. There is deliberately **no** `Derived(inner, project: { event in … })` — no app closure on the demand side, on any platform, ever.

This is not a restriction the manual asks you to respect; it is a shape the API
has. The engine hashes, dedups, refcounts, coalesces, and routes demand off these
descriptors. Two equal fragments can share because their values compare equal.
One opaque closure would make demand sharing, surgical deltas, and
dependency-scoped rerooting unhashable and unexplainable. This is the governing
rule: **values in, code after.** App code remains welcome after delivery.

So what do you do when your projection isn't in the vocabulary? You do **not** reach for a closure — the API won't take one. You extend the vocabulary. Adding a `Selector` variant (or making `kinds` bindable, or adding an `IdentityField`) is a deliberate design event with an adversarial gate, precisely because the vocabulary is forever and every consumer depends on its being closed. In practice the four selectors plus `SetOp` cover the real Nostr read shapes — the resolver's contract suite builds `$myFollows`, NIP-29 groups, follows−mutes, address-coordinate lists, and an unrelated bookmarks shape (`kind:10003` `e`-tags) with *zero* engine changes between them, which is the generality witness that the closed set is wide enough.

## A live query is more than its selection

The `Filter`/`Binding` value describes selection. The canonical query descriptor
also carries source authority and access context because those affect routing,
coalescing, and acquisition evidence. Identical selection subgraphs may share,
but full wire demand shares only when context is compatible.

`$myFollows` is a reusable derived binding, not a new reactive input. A richer
helper such as a NIP-29 current-user-groups query belongs to that protocol
module. Whether apps can register additional named reactive values beyond
`ActivePubkey` remains a provisional surface-design question; no opaque closure
enters the grammar in either case.

---

<!-- nav-footer -->
<sub>← [Packaging & distribution](08-packaging.md) · [Index](README.md) · [Consuming results](10-consuming-results.md) →</sub>
