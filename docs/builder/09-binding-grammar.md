# Live queries & the binding grammar

**Status: BUILT** — the grammar, the resolver, and the Swift/Rust surfaces are shipped and exercised by live tests. Every example below is the real current API.

After this chapter you can express a *reactive* read — "notes from whoever I follow," "the groups I'm a member of," "my follows minus my mutes" — as a single plain value, hand it to `observe`, and let the engine keep it live as your contact list, group memberships, and active account change underneath it. You never watch a `kind:3`, re-issue a `REQ`, or hand-maintain a set. That whole job moves behind the boundary.

## The read noun is a filter whose fields are bindings

A live query is a Nostr `Filter` — `kinds`, `authors`, `ids`, `#`-tags, `since/until/limit` — except that each *field value* is not a literal set but a `Binding`. A binding is one of exactly four things:

```
Binding = Literal(set)                     // a fixed hex/tag-value set
        | Reactive(ActivePubkey)           // re-resolves when the active account changes
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

## Example 1 — `$myFollows` (depth-1)

The canonical reactive read: notes authored by whoever the active account's `kind:3` contact list currently names. This is the falsifier's own flagship query, and it lives in *app* code — NMP exposes only the algebra, never anything named "follows" (see *The batteries: recipes catalog*).

```swift
// apps/Falsifier/Sources/Falsifier/FeedFilters.swift
static func follows(kinds: [UInt16]) -> NMPFilter {
    NMPFilter(
        kinds: kinds,
        authors: .derived(
            inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
            project: .tag("p")
        ),
        limit: 200
    )
}
```

Read it inside-out. The inner filter is `kind:3` events by `Reactive(ActivePubkey)` — *your* contact list, re-rooted automatically whenever you switch accounts. `project: .tag("p")` says "take the `p`-tag values of the matched rows" — the pubkeys you follow. The outer filter asks for `kinds` notes authored by that projected set.

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

What the engine does with it is the point. When your `kind:3` names A, B, C, the resolver opens one *inner* atom (`kind:3` by you) plus one *content* atom per follow (`kind:1` by A, by B, by C). When a newer `kind:3` drops C and adds D, the demand delta is exactly `Close(kind:1 by C)` then `Open(kind:1 by D)` — one atom closed, one opened, everything else untouched. The set-diff, not the event, gates the fan-out: a newer `kind:3` listing the *same* members produces an **empty** delta. This is *recompile-not-reopen*, and it is why the app never sees — and never has to manage — the intermediate follow set.

## Example 2 — the groups I'm in (depth-2, NIP-29)

Reactivity nests. "The groups I'm a member of" is a NIP-29 shape: group-metadata events (`kinds 39000/39001/39002`) whose `#d` (the group id) comes from an inner query — the `kind:39002` membership lists (`#p` tags a member) that name *me*.

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
let feed = NMPFilter(
    kinds: [1],
    authors: .setOp(.diff, [follows, mutes])       // follows − mutes
)
```

`Diff` is "first operand minus the union of the rest"; `Union` and `Intersect` round out the algebra. When you mute someone you already follow, the delta is a single `Close(kind:1 by that author)` — surgical, engine-side, with no app-visible expansion at any point. Publish a mute, and your feed narrows itself.

## The closed-Selector rule: values in, code after

Look again at the one part of a `Derived` binding you might be tempted to make programmable — `project`. It is a **closed vocabulary**, `Selector`, with exactly four members:

```
Selector = Authors | Ids | Tag(name) | AddressCoord
```

`Authors` projects each matched event's author; `Ids` its id; `Tag(name)` a single-letter tag's values; `AddressCoord` the `(kind, author, d)` coordinate (which fans out into one co-pinned atom per coordinate). That is all a projection can be. There is deliberately **no** `Derived(inner, project: { event in … })` — no app closure on the demand side, on any platform, ever.

This is not a restriction the manual asks you to respect; it is a shape the API has. The reason is mechanical: the engine hashes, dedups, refcounts, coalesces, and routes demand off these descriptors. Two apps that build the same `follows` filter share one graph node *because the value compares equal*. One opaque closure anywhere in that path and the whole demand-sharing, surgical-delta, cross-account-reroot story collapses — the closure can't be hashed, can't be compared, can't be introspected on the diagnostics screen. This is the governing rule of the entire read surface: **values in, code after.** App code is welcome, but only *after* delivery, folding delivered rows into your view (see *Delivery-side transforms* and *Consuming results*).

So what do you do when your projection isn't in the vocabulary? You do **not** reach for a closure — the API won't take one. You extend the vocabulary. Adding a `Selector` variant (or making `kinds` bindable, or adding an `IdentityField`) is a deliberate design event with an adversarial gate, precisely because the vocabulary is forever and every consumer depends on its being closed. In practice the four selectors plus `SetOp` cover the real Nostr read shapes — the resolver's contract suite builds `$myFollows`, NIP-29 groups, follows−mutes, address-coordinate lists, and an unrelated bookmarks shape (`kind:10003` `e`-tags) with *zero* engine changes between them, which is the generality witness that the closed set is wide enough.

## Where this leaves you

You now have the whole read grammar. Everything else in Part III is about the *other* side of the boundary: what the engine hands back (*Consuming results*), how to tell "empty" from "not yet known" (*Coverage: empty vs unknown*), and the two PLANNED surfaces that add ordering and post-delivery app code on top (*Feeds & the Collection observation mode*, *Delivery-side transforms*). None of them change the grammar — they consume it.

---

<!-- nav-footer -->
<sub>← [Packaging & distribution](08-packaging.md) · [Index](README.md) · [Consuming results](10-consuming-results.md) →</sub>
