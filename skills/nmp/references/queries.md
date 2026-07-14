# Queries

## Model

A query descriptor is a value, not a callback:

```text
Demand = selection Filter + SourceAuthority + AccessContext + CacheMode
Binding = Literal | Reactive(ActivePubkey) | Derived | SetOp
Selector = Authors | Ids | Tag(name) | AddressCoord
SetOp = Union | Intersect | Diff
```

Filter fields are kinds, authors, ids, indexed single-character tags, since, until, and limit. `Selector::Tag(name)` projects already acquired rows locally and may use arbitrary tag names; that is different from the filter tag map's NIP-01 single-character keys.

Direct Rust `Derived.inner` is a full `Demand`, so the inner query declares its own source/access context. Swift and Kotlin currently accept a bare `NMPFilter` for the derived inner value. Do not write cross-platform examples that hide this difference.

## Source and cache rules

- `AuthorOutboxes` requires an authors binding.
- `Pinned` requires a nonempty relay set and asks only those relays.
- `CacheMode::Strict` matters only with pinned authority; it limits cached rows to provenance intersecting the pinned relay set.
- `AccessContext` currently has only `Public`. It reserves descriptor identity for future AUTH work; it does not mean AUTH is implemented.

## Delivered state

Rust subscriptions deliver row deltas plus acquisition evidence. Swift `NMPQuery` and Kotlin's Flow bridge accumulate those deltas and deliver full `RowBatch` snapshots. A `Row` contains the event and its sorted, deduplicated relay-source set.

Acquisition evidence contains per-source `reconciledThrough` and current link status, plus `NoPlannedSource`, `NoResolvedDemand`, or `LocalLimit` shortfalls. These are scoped facts. They do not prove the Nostr network is complete.

In an app accumulator:

- add or replace by event id when a full row arrives;
- update only sources on a provenance-growth delta at the Rust tier;
- remove by id on retraction;
- apply app-owned sorting and windowing after accumulation.

## Pagination and observation ownership

Changing a filter means observing a new value. It is not mutation of an existing query. When extending a time window, overlap safely and deduplicate by event id; keep the earlier observation only as long as needed for the transition.

Swift observations subscribe eagerly when constructed. Call `cancel()` or release the observation owner. Kotlin flows subscribe on collection; each collector opens a query, so share with `stateIn`/`shareIn` and a lifecycle-bound scope when one query should feed multiple consumers.
