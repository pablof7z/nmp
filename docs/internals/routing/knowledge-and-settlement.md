---
title: "Routing knowledge and settlement"
category: routing
slug: knowledge-and-settlement
status: built
date: 2026-07-29
owns:
  - the neutral three-state author-route fact
  - exact request settlement across NIP-01 and NIP-77
  - the private fact-writer boundary
  - optional protocol assembly
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/outbox.md
  - docs/bug-class-ledger.md
---

# Routing knowledge and settlement

Generic routing knows one atomic author fact:

```text
Unknown
Present { outbound, inbound }
Absent
```

The key is a decoded `PublicKey`. `Present` replaces both directions in one
write; either set may be empty. `Absent` means every exact source carrying the
current question settled without a winning record. It is process-local and a
restart returns it to `Unknown`. A later positive record replaces `Absent`.

## Ownership

`nmp-router` exposes only the read-only `RoutingFacts` trait. It has no
protocol kind, marker, indexer, mutable directory, or lookup-specific write
door. Production `EngineCore` owns the concrete in-memory store and its
borrowed, non-cloneable writer. Only an attached protocol assembly can receive
that writer capability.

Provider needs leave core as one generic `AuthorRouteNeedsChanged` set. Most
entries are `Unknown` inputs. A zero-destination `Auto` also retains all of
its settled contributors as provider needs: a later positive replacement is
the only neutral fact that can unpark it, so naming those authors
`unknowns` would be false and tearing their provider query down would strand
the write. This is a need declaration, not a hidden subscription.
Feature-disabled core therefore contains no NIP-65 dependency or behavior.

A live `AuthorOutboxes` read contributes each resolved author while that
author has no positive outbound route (`Unknown`, `Absent`, or `Present` with
an empty outbound set). A positive outbound route retires that provider work
and becomes the content plan; pinned provider queries never contribute their
own authors back into the set. This lets a cold public query bootstrap without
turning an indexer into a generic content route or keeping discovery work alive
after routing is already usable.

## Exact settlement

The generic evidence fact is `RequestSettled`. It cites the exact path,
filter revision, relay, access context, transport generation, request
revision, observed time, and terminal:

- `Eose` after the exact ordinary NIP-01 request reaches EOSE.
- `Nip77` after successful reconciliation, immediately when no ids are
  missing or only after the missing-id backfill reaches EOSE.

Timeout, refusal, disconnect, cancellation, and abandoned requests never
settle. Neither does a terminal whose exact #816 request-scoped
facts-before-claims transaction was poisoned by a failed EVENT or coverage
commit. A terminal from an old revision or undeclared source cannot settle
the current question.

## Optional NIP-65 assembly

`nmp-nip65` is engine-free. It owns:

- exact kind:10002 demand over operator-selected sources;
- canonical replaceable winner selection before parsing;
- authoritative current-row removal, including atomic predecessor reveal;
- `read`/`write`/unmarked marker parsing and relay admission;
- all-sources settlement and `Present`/`Absent` coordinator updates;
- first-publication bootstrap composition.

The non-default `nmp/nip65` feature binds those pure values to ordinary
`LiveQuery`, observation evidence, and `WriteIntent` values. Its runtime
assembly privately converts coordinator updates into the one atomic core
writer. No protocol vocabulary leaks into the router or generic reducer.

With zero exact sources, no query opens and no new absence can be minted;
current facts remain unchanged.
