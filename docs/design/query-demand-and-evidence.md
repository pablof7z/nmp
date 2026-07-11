# Query demand and acquisition evidence

- **Status:** TARGET CONTRACT - architecture agreed; the current `Filter` plus
  aggregate `Coverage` surface does not yet satisfy it.
- **Owns:** live-query identity, reusable derived demand, snapshot evidence, and
  the boundary between ordinary observations and diagnostics.

## 1. The semantic descriptor

An app observes a demand, not merely a Nostr filter:

```text
Demand := Selection + SourceAuthority + AccessContext
```

`Selection` is the closed filter/binding graph that decides which canonical
store rows match. `SourceAuthority` is typed policy describing which routing
facts may acquire those rows. `AccessContext` carries protocol state that can
change a relay's answer, such as an AUTH identity or visibility grant.

Names and concrete record layouts remain provisional. The invariant is that all
three dimensions participate in descriptor identity, explainability, routing,
wire sharing, and acquisition evidence.

### Safe sharing

- Equal full descriptors may share graph nodes, wire demand, and evidence.
- Equal selections may share resolution and local store matching even when
  source/access context differs.
- Wire filters may share only when the compiler can prove the shared request is
  valid for every participating source/access context.
- Evidence from one source/access context never proves acquisition under
  another.

## 2. Selection remains a closed value language

```text
Binding  := Literal(set)
          | Reactive(CurrentPubkey)
          | Derived(inner: Filter, project: Selector)
          | SetOp(Union | Intersect | Diff, [Binding])
Selector := Authors | Ids | Tag(char) | AddressCoord
```

The exact public spelling may change. These properties may not:

- every node is serializable, hashable, introspectable, and printable;
- selectors are a closed typed vocabulary, never app closures;
- a changed input produces a set diff, not a wholesale graph rebuild;
- an existing observation handle survives recompilation;
- withdrawn demand closes only when no other descriptor still requires it;
- no content kind receives a privileged engine branch.

`$currentPubkey` is one reactive root. Changing it re-resolves only graphs that
reference it. It is not a command to clear other account queries, change every
signer, or partition the cache.

### Reusable fragments and typed protocol queries

Apps and opt-in protocol crates may expose functions that construct a binding
graph. A follows fragment may expand to a `Derived` contact-list filter; NMP sees
and explains the expansion exactly as if the app wrote it directly.

Some protocol operations return richer typed results rather than a field
binding. A NIP-29 helper may expose group references containing the group id and
host relay derived from group protocol events. This is still implemented through
ordinary demand plus a typed protocol projection. It is not an opaque macro,
registered closure, or new acquisition mechanism.

## 3. Snapshot contract

An ordinary query snapshot carries:

1. **Rows:** current canonical store rows matching `Selection`.
2. **Cache evidence:** enough revision/provenance information to identify the
   local state represented by the snapshot.
3. **Acquisition evidence:** compact facts scoped to the descriptor's current
   planned sources.
4. **Shortfall evidence:** explicit local limits or unavailable planned sources
   that prevented the intended acquisition.

The compact evidence vocabulary should report facts, not judgment. Useful facts
include whether a planned source is cached-only, connecting, AUTH-blocked,
requesting, EOSE-observed, reconciled through a watermark, disconnected, or in
error. Exact raw wire filters and counters remain diagnostics.

### No global completeness claim

NMP cannot know every relay that could contain a matching event. Therefore:

- EOSE proves that one relay finished one request, not that Nostr is complete;
- a watermark is per source/window evidence, not authoritative global truth;
- an empty row set is simply empty in the local replica represented by the
  snapshot;
- NMP never emits `synced`, `syncHealth`, global `complete`, or
  `authoritativeEmpty`;
- applications interpret the evidence for their own UX and policy.

Persisted watermarks remain useful for avoiding redundant work and explaining
what the cache previously acquired. They must not be lifted into an unknowable
global proposition.

## 4. Diagnostics boundary

Ordinary snapshots stay compact. The permanent diagnostics stream retains:

- current source plan and its revision;
- exact per-relay wire filters and subscription counts;
- connection, AUTH, EOSE, negentropy, error, and watermark facts;
- events received per relay and kind;
- lane counts, authors served, reverse coverage, and coalescing decisions;
- binding expansion and local limit/shortfall reasons.

Diagnostics is a read-only explanation of the same compiler, store, and
transport state. It must not reconstruct evidence from raw callbacks through a
parallel path.

## 5. Local trust and access context

One engine instance has one shared canonical cache. Accounts inside it are not
mutually isolated users. AUTH/access context is retained because it changes
what was requested and observed, not because matching rows are hidden from
other local queries after validation.

Mutually untrusted local users require an explicit destructive engine reset,
not an implicit account switch.

## 6. Falsification

Before this contract is marked built, tests must show:

- equal selections with different access contexts do not share evidence
  incorrectly;
- changing `$currentPubkey` reroots dependent demand while an unrelated
  multi-account literal query remains live;
- a reusable fragment prints the same graph as its raw construction;
- one source reaching EOSE while another is offline yields per-source facts and
  no global complete state;
- cached rows remain deliverable alongside AUTH, connection, and limit
  shortfalls;
- diagnostics shows the exact plan that produced the compact snapshot evidence.
