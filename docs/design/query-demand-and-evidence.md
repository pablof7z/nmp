# Query demand and acquisition evidence

- **Status:** PARTIAL CONTRACT. The current `Filter` selection graph now ships
  with per-current-plan `AcquisitionEvidence` across Rust, FFI, Swift, and
  Kotlin; the former query-level aggregate is gone. Full
  `Selection + SourceAuthority + AccessContext` identity, persistence, and
  context-safe wire sharing remain TARGET under #49.
- **Owns:** live-query identity, reusable derived demand, snapshot evidence, and
  the boundary between ordinary observations and diagnostics.

## 1. The semantic descriptor and per-handle policy

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

An observation also carries two orthogonal, per-handle policies that do not
participate in that semantic identity:

```text
CacheMode := Agnostic | Strict
Freshness := Live | MaxAge(seconds: u64) | CacheOnly
```

`Live` is the default and preserves cache-then-live behavior. `CacheOnly`
projects the canonical cache and contributes no remote work under every
condition. `MaxAge` performs one opening-time check over existing store
coverage. It suppresses this handle's remote work only when every atom in the
full resolved subtree has fresh coverage from every relay session assigned by
the same router/directory/admission/cap path that would plan the candidate as
live. Missing routing, cap shortfall, missing coverage, a coverage floor above
the atom's requested floor, or any stale assigned relay degrades the handle
once to ordinary `Live` for its lifetime.

The opening check is currently conservative for a query whose `until` is
already older than the `MaxAge` cutoff: it still requires coverage through the
cutoff, while honest attribution caps `through` at that sent `until`. Such a
handle therefore becomes `Live` rather than claiming suppression from a
special bounded-past rule. No broader bounded-window freshness semantics are
implied by `MaxAge` today.

Freshness is coverage of the question, not event presence. A recently covered
empty result is fresh. `MaxAge` deliberately accepts that a newer replaceable
event may exist remotely within the tolerated interval. A satisfied handle
remains suppressed even after its evidence ages; a new handle makes a new
decision. No timer, shared coordinator, polling lane, or new persistence state
exists.

Coverage time remains the attribution ruling's send-shape/completion-clock
fact: `through = min(engine wall clock at EOSE/NEG completion, until captured
when the request was sent)`. Event `created_at` is not an attribution input, so
a hostile future-dated event cannot manufacture freshness.

### Safe sharing

- Equal full descriptors may share graph nodes, wire demand, and evidence.
- Equal selections may share resolution and local store matching even when
  source/access context differs.
- Wire filters may share only when the compiler can prove the shared request is
  valid for every participating source/access context.
- Evidence from one source/access context never proves acquisition under
  another.
- Handles that differ only in cache/freshness policy share acquisition identity
  but keep independent projection and wire-contribution decisions.

## 2. Selection remains a closed value language

```text
Binding  := Literal(set)
          | Reactive(CurrentPubkey)
          | Derived(inner: Filter, project: Selector)
          | SetOp(Union | Intersect | Diff, [Binding])
Selector := Authors | Ids | Tag(name: String) | AddressCoord
```

`Tag`'s `name` is an arbitrary event-tag key (#64): it projects
already-acquired events locally, so it is never restricted to a single
letter -- distinct from a wire/local `Filter`'s indexed tag keys, which stay
exactly NIP-01's single-ASCII-letter alphabet. The exact public spelling may
change. These properties may not:

- every node is serializable, hashable, introspectable, and printable;
- selectors are a closed typed vocabulary, never app closures;
- a changed input produces a set diff, not a wholesale graph rebuild;
- an existing observation handle survives recompilation;
- withdrawn demand closes only when no other descriptor still requires it;
- no content kind receives a privileged engine branch.

### Projected values carry routing evidence

`Tag("e")`, `Tag("a")`, and `Tag("p")` project a `(value, routing evidence)`
fact rather than discarding the relay context around the value. A valid tag
relay hint is the primary fact. When no valid hint exists, every relay in the
source row's observed provenance is retained as fallback. `AddressCoord`
retains the same source provenance for its coordinate. Other arbitrary tag
selectors remain value-only.

Evidence is keyed by projected value. Union and intersection union the facts
from every surviving path; difference retains the first operand's facts for
surviving values. The outer atom carries the resulting evidence through router
compilation. The engine applies discovered-relay admission before those facts
become candidates. Live atom identity includes evidence so a later observation
can re-route exactly; durable coverage identity erases it because route choice
must not fragment proof for the same selection/source/access tuple.

Derived depth is the number of `Binding::Derived` edges on a path. `SetOp` is
a same-level combinator and adds zero Derived hops.

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

A coverage-satisfied `MaxAge` snapshot retains the exact opening-time plan that
justified suppression and reports those scoped watermarks. `CacheOnly` does not
borrow a live sibling's plan or evidence. Neither is relabeled as global truth.

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
