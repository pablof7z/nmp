# Tracing demand through the compiler

One observed `Demand` becomes local matching plus a concrete source/wire plan.
Diagnostics exposes every stage.

## The pipeline

```text
observe(Demand { selection, source, access })
    |
    | resolve selection bindings
    v
concrete selection atoms + dependency provenance
    |
    | apply typed source authority and access context
    v
source intents + candidate facts
    |
    | solve coverage/caps and retain shortfall
    v
per-source narrow requests
    |
    | safe sharing and widen-only wire coalescing
    v
exact relay REQ plan
    |
    | verify ingest, canonical-store mutation, local re-filter
    v
QuerySnapshot { rows, cache, acquisition, shortfall }
```

The compiler may be incremental, but it is deterministic for the same
descriptor, store revision, source facts, access context, and configured limits.

## 1. Resolve the selection graph

`Literal`, `Reactive(CurrentPubkey)`, `Derived`, and `SetOp` produce concrete
filter atoms. Each projected value retains dependency provenance so a change can
add/withdraw only affected demand.

This stage decides what rows match. It does not grant authority to contact a
relay merely because an `authors` field happens to exist.

## 2. Apply source and access authority

The descriptor explicitly says where acquisition may come from:

- `AuthorOutboxes` authorizes NIP-65 discovery/coverage for selected authors;
- an opaque protocol-host authority authorizes its validated relay/object;
- a private-protocol context authorizes verified recipient inbox facts; and
- operator bootstrap policy authorizes discovery lanes.

Access context says which AUTH identity or visibility grant applies. Equal
selections under different access contexts retain separate evidence unless the
compiler can prove sharing safe.

Filter shape is never authority. An authorless selection is not automatically
"pinned," and a selection with authors is not automatically an outbox request.

## 3. Solve candidates under limits

Where one authority supplies multiple candidate relays, NMP selects a
deterministic bounded covering set. The current policy may seek redundant
coverage, but exact defaults are implementation policy.

Every cap outcome is explicit:

- no candidate facts;
- fewer candidates than the requested redundancy;
- fan-out cap exhausted; or
- access/source unavailable.

The engine never unions every known relay without a cap and never presents a
capped subset as complete acquisition.

## 4. Coalesce only when semantics are preserved

Exact duplicate wire requests share automatically. A merge rule may widen
compatible filters only when local re-filtering guarantees every original
selection remains exact.

Context participates in the proof. Two equal filters cannot share evidence or a
wire request if their AUTH/source semantics make that unsafe.

If a merge rule cannot prove widening, the requests remain separate and
diagnostics records the decision.

## 5. Diff the wire plan

Stable subscription identity lets a changed derived set update only the relay
requests affected by its set difference. Unchanged sources and descriptors stay
open.

The wire plan records:

- exact filter JSON;
- source/access attribution;
- descriptor references;
- route lane/reason; and
- plan revision.

Apps never own the relay subscription ids.

## 6. Verify, store, and match locally

Inbound events pass the transport verification boundary and canonical store.
Dedup merges provenance; replaceable/delete/expiry rules decide current rows.

Wire coalescing may deliberately overfetch. Before a row reaches one query,
NMP matches it against that descriptor's original selection. Access context
remains attached to acquisition planning and evidence; after validation it does
not hide a matching row in the engine's shared local trust domain. Widen on the
wire, exact at local selection.

## Worked shape

An app-owned derived index might resolve from:

```text
ids := Derived(
  inner: Demand {
    selection: kinds:[appIndexKind], authors:[CurrentPubkey],
    source: AuthorOutboxes,
    access: Public
  },
  project: Tag(e)
)
outer.source := AuthorOutboxes
outer.access := Public
```

Diagnostics can show the inner expansion, projected ids, author-outbox source
facts, selected relays, exact coalesced filters, received counts, per-source
EOSE/watermarks, and any uncovered shortfall. The app never holds the projected
id set or relay grouping. Inner and outer source/access facts remain distinct;
neither demand silently inherits the other's context.

## What to inspect when rows are missing

1. Did the binding graph resolve the values you expected?
2. Which typed authority produced each source intent?
3. Were candidate facts missing or capped?
4. Did access/AUTH block the request?
5. What exact filter reached the relay?
6. Did verified events arrive and enter the canonical store?
7. Did local matching exclude them from this descriptor?

That sequence locates the owning layer without a fabricated health score.

---

<sub>[Index](README.md) · Related: [Source and routing context](17-relays.md) · [Diagnostics](22-diagnostics.md) · [Evidence without completeness](11-coverage.md)</sub>
