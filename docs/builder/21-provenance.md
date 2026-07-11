# Provenance, and why private events can't be republished

**Status: BUILT** — provenance-through-dedup and the fail-closed narrow-only private route are live and verified (`nmp-router/src/{route,coalesce}.rs`, `nmp-engine/src/outbox/mod.rs`, `nmp-engine/src/core/mod.rs`; ledger #5/#6 marked verified at M3).

After this chapter you will understand what provenance is, why it survives deduplication, why a private route is a type with *no widen operation*, and why the engine structurally cannot re-broadcast a private event it received.

## Provenance is a durable field, not a computed guess

Every event the engine stores or routes carries **provenance** — the record of *where it came from* and *why a relay is in its plan*. On the routing side this is `RouteProvenance`: for each wire REQ, which relay, via which lane, covering which authors, produced by the solver or pinned by a fact:

```rust
pub struct RouteProvenance {
    pub relay: RelayUrl,
    pub lane: Lane,                          // Nip65Write, DmInbox, GroupHost, …
    pub covers_authors: BTreeSet<PubkeyHex>,
    pub route_kind: RouteKind,               // OutboxSolved | Pinned
}
```

Every wire REQ traces back to one or more of these. This is the routing half of **bug-class ledger #3** ("every explicit route carries typed provenance") and ledger #4 ("no connection outside a solver-produced plan") — a relay is never in a plan for a reason the engine cannot name.

On the storage side, provenance is a *field of the stored row* — the set of relays an event was observed from (`RelayObserved`). The load-bearing property: **provenance survives deduplication.** When the same event arrives from two relays, the engine does not keep the first and discard the second — it merges the provenance of the duplicate into the existing row *before any other processing*:

> Insert merges provenance on duplicate id before any other processing; provenance is a field of the stored row. Ids/signatures never re-derived post-verification. No API returns an event without retained provenance. — ledger #5

So one row ends up recording *both* relays it was seen on. This is **bug-class ledger #5 (dedup or provenance loss)**, verified live: the same event from two real relays produces one row that remembers both origins. Ids and signatures are never re-derived after the initial verification — the engine trusts its own verified copy and never re-hashes.

Coalescing preserves this too. When the widen-only merge rules fold two atoms into one wire REQ (see *"Where did my query go?" Tracing demand through the compiler*), both operands' provenance lists are concatenated into the merged filter — the merge never drops the record of why either atom existed. The same threading carries each atom's coverage key (`absorbed`), so provenance and coverage attribution both survive every merge.

## Private routes are `NarrowOnly` — a type with no widen operation

A private event (a NIP-17 gift-wrap, a DM) must go to a *specific, narrow* set of relays — the recipient's inbox — and to **nowhere else**. Leaking it onto a public write relay would expose metadata the whole point of the private route is to hide. NMP makes that leak *unrepresentable* rather than merely discouraged.

The private routing class carries its relay set as a `NarrowOnly<RelayUrl>`:

```rust
pub struct PrivateRoute {
    pub relays: NarrowOnly<RelayUrl>,
}

pub struct NarrowOnly<T> { items: BTreeSet<T> }

impl<T: Ord> NarrowOnly<T> {
    pub fn new(items: impl IntoIterator<Item = T>) -> Self { … }  // the ONLY constructor
    pub fn is_empty(&self) -> bool { … }
    pub fn iter(&self) -> … { … }
    // NOTE: there is no insert, no extend, no union, no widen.
}
```

Read the impl block and notice what is *absent*. `new` is the only way to populate the set — a one-shot, fixed set fixed at construction, by a caller that has *already* resolved and narrowed it. After construction there is **no operation that adds a relay**. No `insert`, no `extend`, no `union`. This is **bug-class ledger #6 (private-event republish)** as a type property: "a private route type has no widen operation."

Because the type cannot widen, the fail-closed behavior falls out for free. An empty `NarrowOnly` set is exactly how "unroutable private recipient" is expressed — and the reducer fails it *closed*:

```rust
WriteRouting::PrivateNarrow(route) => {
    if route.relays.is_empty() {
        Err("private route has no narrow relay set -- fails closed, \
             never widens to a public relay".into())
    } else {
        Ok(route.relays.iter().cloned().collect())
    }
}
```

An empty private route resolves to a whole-intent `WriteStatus::Failed` — before any relay is ever contacted — and **no `PublishEvent` effect is emitted for any relay**. Structurally, an unroutable private recipient cannot reach the wire, because the `relays` binding is never populated in that branch and there is no operation that could hand it a public fallback. Compare with `AuthorOutbox`, which *does* consult the directory and fan out; `PrivateNarrow` never consults the directory at all — its relay set is exactly and only what the caller pre-narrowed.

The engine's own note (`resolve_routes`) states it plainly:

> `PrivateNarrow` never consults the directory at all — its relay set is exactly whatever the caller pre-narrowed into the `NarrowOnly` set, empty or not (ledger #6's fail-closed mechanism).

## Why a received private event can't be re-broadcast

Put provenance and `NarrowOnly` together and the headline guarantee emerges. Suppose your app receives a private event — a gift-wrapped DM addressed to you. Its provenance records it came in over a `DmInbox` lane. Could your app, accidentally or maliciously, re-publish it to a public relay?

No — and not because a rule says "don't." There is simply no path:

1. **Public routing requires a public routing class.** Re-broadcasting would mean handing that event to `AuthorOutbox` or `ToInboxes` and letting the outbox fan it out to write relays.
2. **A private event's route is a `PrivateNarrow`, and `PrivateNarrow` cannot widen.** There is no operation that turns a narrow private route into a wider public one — the `NarrowOnly` type has no widen op, so there is no in-crate path from "private route" to "public relay set."
3. **Provenance survives, so the engine never loses track of what a received event is.** Because dedup merges rather than discards, and provenance is a durable field, a received private event carries its origin the whole way through; it is never laundered into looking like a fresh authored event.

So the engine *refuses* to re-broadcast a received private event — the refusal is the absence of any operation that could do it. This is ledger #6's live-verified claim: "empty resolved private set → whole-intent `Failed`, zero publish effects (structurally unreachable to a relay)," combined with the no-widen type. The write path has exactly three routing classes, and the only private one is a dead end for widening by construction.

## Republishing to a *recomputed narrow* set is fine

One legitimate case looks superficially like republishing: you hold a validly-signed private event and want to send it to a *freshly recomputed* narrow relay set (the recipient moved inboxes, say). That is allowed — because it is still narrow. The write payload can be `Signed` (skip the signer, signing and publishing are orthogonal), and the routing is a *new* `PrivateNarrow(NarrowOnly::new(recomputed_inboxes))`. You are not widening an existing route; you are constructing a fresh narrow one. The type permits constructing narrow sets all day; what it forbids is *widening* one, and it never lets a narrow route spill into a public relay.

## Reading provenance on the diagnostics surface

Provenance is legible, off the data path. The diagnostics `byLane` breakdown per relay tells you *which lane* put each subscription there, and the exact wire `filters` tell you what was asked — so you can confirm a private/DM lane never appears on a public write relay's plan (see *Diagnostics & debugging*). If a private route ever showed up routed to a public relay, the lane breakdown would name it; it can't, but you can *check* that it can't, which is the whole ethos of the ledger.

## The guarantees, named

| Guarantee | Ledger | Mechanism |
|---|---|---|
| Same event from two relays → one row, both origins kept | #5 | provenance merged on duplicate insert, before other processing |
| Ids/signatures never re-derived after verification | #5 | no re-hash path; verified copy trusted |
| Private route can't widen to a public relay | #6 | `NarrowOnly` has no widen/insert/union op |
| Unroutable private recipient fails closed, never falls back | #6 | empty set → whole-intent `Failed`, zero `PublishEvent` |
| Received private event can't be re-broadcast | #6 | no operation converts a `PrivateNarrow` into a public route |

---

<!-- nav-footer -->
<sub>← [Capabilities](20-capabilities.md) · [Index](README.md) · [Diagnostics & debugging](22-diagnostics.md) →</sub>
