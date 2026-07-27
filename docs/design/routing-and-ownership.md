# Relay routing + kind ownership — the canonical spec

- **Date:** 2026-07-11
- **Status:** Owner-confirmed default routing policy (Part A) — settled; do not re-litigate its rules, only their mechanics. **Parts B and C were rewritten 2026-07-27 by #859:** the route-override primitive and the global kind-ownership registry they specified are DELETED, not deferred. `crates/nmp-ownership` and `crates/nmp-audit`, and every protocol crate's `claims()` export, no longer exist; #757 and #758 (which would have wired the registry into routing) are closed NOT_PLANNED. Parts B/C now record what actually holds and name the one enforcement gap that remains open (§4.3). §8's remaining owner-decision list is scoped to Part A. Provisional-until-v2 like everything else.
- **2026-07-11 promotion correction:** ownership below is **schema ownership**, not
  ownership of every event that participates in a protocol context. A NIP module
  claims only the exact event schemas that NIP defines. Per-publication context
  contributed to a foreign-owned unsigned draft is a separate, typed operation
  (§3.2.1); it never expands what schemas that module defines.
  Likewise, historical "no `relays:` parameter" wording means no untyped route
  override on the default path; a live query may carry explicit typed source
  authority as specified in `query-demand-and-evidence.md`.
- **2026-07-27 write-boundary correction (#839):** the generic
  `ToInboxes(recipients)` route described below is removed. An independently
  supplied recipient array cannot prove that the event schema owns those
  recipients or that its body agrees with them. Recipient delivery may return
  only through a protocol-owned operation that fixes the complete body,
  recipient meaning, bounded derivation, and route together. Sections 2.3–2.6
  retain the earlier design context, but their generic p-tag inbox fan-out is
  superseded by #839; #842/#843 own the remaining safe-write/custom-schema
  boundary.
- **Anchors:** VISION P4 (routing is the mission, not optional), P5 (widen-only), §10 ("values in, code after"), bug-ledger #3/#4/#6; `docs/known-gaps.md` "DM inbox routing incorrect (M3-D)" (the `ToInboxes` gap this spec closes).
- **Code ground truth:** `crates/nmp-router/src/{facts,route,router,solver,coalesce,plan,deliver,diag}.rs`, `crates/nmp-engine/src/core/mod.rs` (`sync_discovery`, `resolve_routes`, write outbox), `crates/nmp-engine/src/outbox/mod.rs`.

---

## 1. What is already built (the substrate this extends)

Everything below is shipped and tested; this spec extends it, it does not replace it.

| Built | Where |
|---|---|
| Closed `Lane` vocabulary (`Nip65Write`, `Hint`, `Provenance`, `UserConfigured`, `IndexerDiscovery`, `GroupHost`, `DmInbox`); every relay-bearing fact is lane-tagged | `facts.rs::Lane`, `LanedRelay` |
| `RelayDirectory` trait (write/extra/indexers/pinned + `ingest_write_relays`) and `LiveDirectory` (self-bootstrapping: starts with indexers only, learns kind:10002 at runtime) | `facts.rs` |
| `DiscoveryKinds` = `{0, 3} ∪ 10000..=19999` (owner-affirmed); indexer relays eligible ONLY for discovery-kind atoms, never a content fallback | `facts.rs::DiscoveryKinds`, `route.rs::build_candidates` |
| **Additive relay roles**: a relay that is both an author's write relay and an indexer gets both roles' atoms (unioned candidates, never one-role-per-relay) | `router.rs::additive_relay_roles_union_not_exclusive` |
| 2-relay-min + cap greedy deterministic coverage solver with typed `Shortfall` (`NoCandidates` / `FewerCandidatesThanK` / `CapExhausted`) | `solver.rs` |
| Widen-only coalescing (`StructuralUnion` — one rule over every array axis; unproven rules dropped, ship separate) + local re-filter on delivery | `coalesce.rs`, `deliver.rs` |
| Per-relay REQ partitioning, skeleton-stable `SubId` (author churn = one overwriting REQ), full-recompile-then-surgical-diff | `plan.rs`, `router.rs` |
| Read-side typed provenance: every `WireReq` carries `Vec<RouteProvenance>` (relay, lane, covered authors, `OutboxSolved`\|`Pinned`) — no wire REQ without a traceable route | `route.rs::RouteProvenance` |
| Self-bootstrapping outbox: `sync_discovery` opens a widen-only internal kind:10002 sub against indexers for authors with unknown write relays (wave 2 supersession) | `core/mod.rs::sync_discovery` |
| Write outbox: durable `WriteIntent` → `WriteStatus` stream; app-facing routing is `AuthorOutbox` only; closed protocol operations can mint withheld `PrivateNarrow` or relay-list bootstrap routes; `NarrowOnly<T>` has no widen operation | `outbox/mod.rs`, `core/mod.rs::resolve_routes` |
| Diagnostics: per-relay sub counts, by-lane counts, reverse coverage, exact filters, uncovered authors, dropped rules | `diag.rs` |

Known deviations this spec resolves: the solver counts indexer/extra candidates toward the 2-min; there is no app-relay or fallback-relay concept; write routing is caller-chosen rather than policy-derived; no per-kind override seam; no kind-ownership boundary. The former generic recipient-routing deviation was removed outright by #839 rather than repaired.

---

## 2. Part A — The default routing policy (owner-confirmed)

### 2.1 Relay lanes — operator policy, set once

Three **app-configured lanes** join the author-derived facts. They are operator policy declared at engine construction (`NmpEngineConfig`), never per-query — so ledger #3's "no `relays:` parameter" holds untouched. All three are **additive**: they never replace outbox routing and **never count toward the 2-relay-min** (only an author's own kind:10002 relays count toward coverage).

| Lane | Reads | Writes | Counted toward 2-min? |
|---|---|---|---|
| `indexerRelay` (≥0, BUILT) | Discovery kinds only (`DiscoveryKinds`), all authors, always | Kind:10002/other discovery-kind self-publishes MAY additionally go here (owner default: yes — an indexer that can't see your relay list can't bootstrap you) | Never |
| `appRelay` (≥0, NEW) | **Everything** — all kinds, all authors, always, additive | **Everything** — every default-routed write also goes here | Never |
| `fallbackRelay` (≥0, NEW) | Top-up: fires per-author when that author's own-relay count `< 2` (0 or 1), **and only if no `appRelay` is configured** (appRelay suppresses fallback entirely) | Same rule on the write side | Never |

New `Lane` variants: `AppRelay`, `Fallback`. New `RelayDirectory` accessors (additive, default empty): `app_relays() -> Vec<RelayUrl>`, `fallback_relays() -> Vec<RelayUrl>`, plus `read_relays(&PubkeyHex) -> Vec<LanedRelay>` (§2.4).

**Solver change (the one semantic change to proven code).** Today `build_candidates` folds indexers into per-author candidate lists and the solver counts them toward `k`. Under this spec the solver's `CoverageInput.candidates` contains **only the author's own relays** (`Nip65Write` + `Hint`/`Provenance` extras — see §8 item 3 for whether extras count); indexer/app/fallback relays are applied **outside** the solve, as unconditional additive routes. `Shortfall` keeps its exact meaning ("this author's own relays don't reach k") and becomes the **trigger for the fallback lane**: `fallbackRelay` routes exactly the shortfall authors' atoms (reusing `Coverage.shortfall`, already computed and diagnostics-visible), suppressed when any `appRelay` exists. `FewerCandidatesThanK`/`NoCandidates` remain reported even when fallback tops the author up — fallback is a lane, not coverage.

### 2.2 READ routing (query kinds K, authors A)

For each demand atom, in one compile:

1. **Indexer lane**: if the atom is discovery-kind (`DiscoveryKinds::is_discovery`), route to every `indexerRelay` — all authors. (BUILT.)
2. **App lane**: route the atom to every `appRelay`, always — all kinds, all authors. (NEW.)
3. **Author outbox**: coverage-solve each author's own WRITE-marked kind:10002 relays, 2-relay-min, capped, greedy-deterministic. (BUILT, minus the candidate-set narrowing above.)
4. **Self-bootstrap (wave 2)**: authors whose write relays are unknown get an internal widen-only kind:10002 sub against the indexers; when it lands, the same recompile re-routes their content atoms to their real relays. (BUILT: `sync_discovery`.)
5. **Fallback lane**: authors whose achieved own-relay coverage `< 2` route their atoms additionally to every `fallbackRelay` — **iff no appRelay is configured**. (NEW.)

An author with zero known relays and no app/fallback lane routes nowhere (never an indexer content-fallback) until wave 2 resolves — unchanged.

### 2.3 WRITE routing (event E: kind, author, p-tags)

> **Superseded boundary (#839):** core no longer derives inbox fan-out from
> arbitrary p-tags and no generic `ToInboxes` route exists. The numbered model
> below records the earlier target, not a constructible current API. A selected
> protocol operation must establish recipient semantics together with its
> complete body before it can mint any recipient delivery.

The default write route is **derived from the event**, not chosen by the caller (see §2.5). Union of:

1. **Author's WRITE-marked relays** (kind:10002), all of them (a write fans out to every known write relay; no coverage solve — BUILT in `resolve_routes::AuthorOutbox`), 2-min semantics only in the fallback trigger sense below.
2. **p-tagged recipients' INBOX relays** — each recipient's **READ-marked** kind:10002 relays (2 each, same top-up trigger), **unless** any of:
   - E has **more than 10 p-tags** (11+ ⇒ skip inbox fan-out entirely; **exactly 10 still publishes** — this is a broadcast-spam guard, not a mention limit),
   - E is **kind:3** (a contact list p-tags everyone you follow; it is not addressed *to* them),
   - E is **kind:1xxxx** (10000–19999; replaceable lists p-tag their members, same non-addressed semantics).
3. **appRelay** — always, additive.
4. **fallbackRelay** — for the *author* side when the author's own write relays number `< 2`, iff no appRelay. (Recipient-side under-min top-up is NOT done from the sender's lanes — you don't get to spray someone else's inbox to your fallback relay; an under-min recipient simply gets fewer inbox copies. This is the same trust rule as Part B's fail modes.)

### 2.4 The READ-marked / WRITE-marked kind:10002 distinction (closes the `ToInboxes` gap)

> **Current scope (#839):** the read/write-marked NIP-65 distinction remains
> useful directory truth, but generic writes no longer consume it for
> recipient routing. A future protocol-owned recipient operation may use
> read-marked inbox facts only under its own complete typed contract.

The distinction now matters on both sides: reads consume authors' **write**-marked relays; the write path's p-tag inbox fan-out consumes recipients' **read**-marked relays. Mechanics:

- `parse_nip65_write_relays` (engine) already drops `"read"`-marked entries. Add `parse_nip65_read_relays` (unmarked = both, per NIP-65).
- `RelayDirectory` grows `read_relays(&PubkeyHex) -> Vec<LanedRelay>` (lane: new `Nip65Read`) and `ingest_read_relays` (additive default no-op, mirroring `ingest_write_relays`); `LiveDirectory` stores both sets from the same kind:10002 winner in one `ingest` pass.
- `resolve_routes`' inline-flagged deviation (union of recipients' *write*+extra relays) is deleted, replaced by `read_relays` — recipient discovery for unknown recipients rides the existing `sync_discovery` machinery (kind:10002 is one event covering both sides; no second discovery sub).

### 2.5 Who picks `WriteRouting`

> **Current scope (#839/#842):** FFI, Swift, and Kotlin can construct only
> `AuthorOutbox`; direct Rust retains closed routes needed by protocol modules.
> The raw event-body constructor itself remains pending removal under #842, so
> the `Default` design below is not current authority.

The earlier design proposed **`WriteRouting::Default`**: the engine would derive
§2.3's union from the signed event at route-resolution time, with
`AuthorOutbox` and `ToInboxes` as internal outputs. That proposal is
superseded. `ToInboxes` is deleted, and `PrivateNarrow` remains a fail-closed
route minted only by the layer that owns the complete operation's destination.
The app-facing direction remains an event plus durability with no raw `relays:`
parameter, but #842 owns the remaining constructor hard cut.

### 2.6 Acceptance scenarios (these are the tests)

**A1 — kind:30023 feed.** appRelay = `my-app-relay`. u1 writes to {relay1, relay2}, u2 to {relay2, relay4}, u3 to {relay1, relay4}. Query `kinds:[30023], authors:[u1,u2,u3]` compiles to exactly: `my-app-relay` ← all three authors (app lane, additive); `relay1` ← [u1,u3]; `relay2` ← [u1,u2]; `relay4` ← [u2,u3] (coverage-solved, author-union-coalesced, one REQ per relay). No fallback fires (appRelay suppresses; all authors at 2). Diagnostics show `by_lane` counting `AppRelay` separately from `Nip65Write`.

**A2 — kind:0 two-wave reactive flow.** Query `kinds:[0], authors:[uX]`, uX unknown. Wave 1: kind:0 is a discovery kind ⇒ routes immediately to indexers (+appRelay); a possibly-stale kind:0 renders. In parallel `sync_discovery` widens its kind:10002 sub with uX. Wave 2: uX's 10002 lands ⇒ same recompile routes the kind:0 atom additionally to uX's own write relays (skeleton-stable sub-id ⇒ overwriting REQ, no churn); the store's replaceable supersession makes the fresher kind:0 the winner. Nothing is torn down; the app saw one live query throughout.

**A3 — p-tag publish.** appRelay = `my-app-relay`. u1 (writes to {r1, r2}) publishes kind:1 p-tagging u2 (read-marked inbox {r2, r4}). Route = {`my-app-relay`, r1, r2, r4} — r2 appears once with both roles' provenance (author-write ∪ recipient-inbox; additive roles, same as the read side). The same event with 11 p-tags routes to {`my-app-relay`, r1, r2}; with exactly 10, inboxes still fan out. The same event as kind:3 routes to {`my-app-relay`, r1, r2} regardless of p-tag count.

---

## 3. Part B — Route authority is derived, not overridden (DELETED design)

**2026-07-27 correction (#859).** This section previously specified a
`RoutePolicy` override primitive — `RelaySource`, `AppLanes`, `FailMode`, and a
typed `RouteClass` provenance enum — attached to a module's `KindClaim` and
dispatched from a global `kind -> RoutePolicy` table. That primitive existed as
types in `crates/nmp-ownership` and was **never consumed by any runtime path**.
It has been deleted, along with the crate. The direction that would have wired
it into generic routing (#758) is closed NOT_PLANNED, as is its predecessor
#757: ordinary callers no longer select routing at all, so there is no caller
whose selection a global policy table would have to override.

### 3.1 What replaces it: nothing app-facing

There is no route-override primitive, and no plan for one. Route resolution
derives the route **inside the owning layer** from the complete facts of the
operation or query:

- **Reads.** A live query carries its own typed source/access authority as part
  of its closed descriptor (`query-demand-and-evidence.md`). Where an operation
  needs an exact host or a private source, that authority is part of *that
  query*, and it cannot change the routing of an unrelated query that happens
  to mention the same numeric kind.
- **Writes.** The publishing layer derives the destination from the complete
  write facts (Part A §2.3). Where an operation needs a destination that author
  outbox facts cannot produce, the destination is bound by the operation that
  mints the write, and that authority is operation-local — it does not install a
  policy for every event of that kind.

Neither direction consults a global kind table. Generic router/engine code does
not branch on protocol module identity.

### 3.2 What this rules out

- No raw `relays:` parameter on observe or publish (unchanged — this was always
  the rule, and deleting `RoutePolicy` does not weaken it).
- No standalone "register a route policy" API, because there is no route policy
  noun at all.
- No global `kind -> policy` dispatch, and therefore no per-kind `match` in core
  either. Core knows NIP-01/NIP-65 defaults and nothing else.

### 3.2.1 Contextual publication is not schema ownership

This doctrine survives the deletion and is still load-bearing (bug-class ledger
row 14; cited from `nmp-nip29` and `nmp-media`).

Some protocols constrain an event without defining its schema. NIP-29 is the
forcing example: a group-bound publication may carry an event whose schema is
defined by another crate, while NIP-29 contributes the required `h` tag and the
group's host relay.

That operation is **per unsigned draft**, not a route override for the draft's
kind:

1. The schema owner builds and validates an immutable unsigned draft.
2. The contextual module returns a new draft containing only its protocol
   contribution, plus typed route context scoped to that publication.
3. The core validates the composed draft, selects the default signer or an
   explicit signer override, signs exactly once, and publishes through the
   ordinary outbox and receipt machinery.

The contextual module cannot rewrite schema-owned fields, define the foreign
kind's schema, or install a kind-wide route override. A pre-signed event is
immutable and therefore cannot acquire missing group context; it can only be
published verbatim.

The important consequence, and the reason ledger row 14 was a TARGET: a route
contribution is **not** gated on owning the kind. The deleted design gated every
contribution on ownership, which is precisely the confusion the row names.

### 3.3 Typed route provenance — the `RouteClass` decision, reversed

The 2026-07-11 spec adopted a write-side `RouteClass` enum (`Automatic`,
`HostPinned`, `VerifiedPrivateInbox`, `Manual`, `Imported`, `Diagnostic`) minted
by a policy engine and threaded through `WriteStatus::Routed`. It was never
threaded through anything: the enum lived in `nmp-ownership` with no producer
and no consumer. It is deleted with the crate.

The accountability argument behind it stands, and is met structurally rather
than by a provenance tag: an unroutable write fails closed and its receipt
carries the shortfall as durable evidence (ledger rows 6 and 16), and the read
side keeps its existing `RouteProvenance{lane, route_kind}`. If a future
operation genuinely needs a write-side trust-regime label, it is minted by the
layer that owns that operation's destination — not by a global table keyed on a
number.

---

## 4. Part C — Schema ownership is structural, not registered

**2026-07-27 correction (#859).** This section previously specified a global
ownership registry: `KindClaim`/`KindScope`/`ModuleId`, a `ClaimSet` fold, a
`ModuleRegistration` list taken at engine construction, and a workspace audit
crate (`nmp-audit`) that folded every enrolled module's `claims()`. Layers 1 and
3 were never built; layer 2 was built and was **self-enrolled** — it discovered
only crates that voluntarily declared a normal dependency on `nmp-ownership`, so
a new protocol crate evaded the alleged workspace guarantee simply by omitting
that dependency, and non-claiming crates were kept in the registry by prose-only
`DeclaresNoClaims` entries. A proof a new author can opt out of is
governance-by-policing, which this repo's ledger does not accept as a mechanism.
All of it is deleted: both crates, every `claims()` export, and every
audit-only dependency edge.

### 4.1 What ownership means now

**Schema ownership is a dependency fact, not a declaration.** A protocol crate
owns a schema because it is the crate that defines, builds, and parses it, and
because every other crate that touches that schema does so by depending on it
and consuming its typed output. `nmp-nip51` owns kind:10009 because
`nmp-nip29` reads `SimpleGroupEntry` from it rather than re-parsing tags;
`nmp-blossom` owns kind:24242 and `nmp-nip68` kind:20 because `nmp-media`
composes their outputs rather than reconstructing them. Nothing has to be
registered for that to be true, and nothing can be forgotten to make it false.

**Publication authority stays with the layer that mints the write.** A supported
write is constructible only through a typed semantic operation; the numeric kind
is not the authority boundary, the complete operation is.

**Two crates parsing the same numeric kind is not, by itself, a conflict.** The
dangerous duplicate is two public operations claiming the same semantic
responsibility, or generic core code branching on protocol meaning — neither of
which a table of numbers detects.

### 4.2 What generic core must never do

1. Take a module registration list, in any form.
2. Dispatch on protocol module identity.
3. Look up a global kind -> policy/owner table at route resolution.
4. Contain per-kind protocol knowledge beyond the NIP-01/NIP-65 defaults Part A
   specifies.

These are the properties reviews and falsifiers check; they are stated as
absences because the mechanism is the absence of the noun.

### 4.3 The one remaining enforcement gap — TARGET

Where a **dependency direction** is the invariant (e.g. "no pure protocol crate
depends on the engine/router/store/transport merely to describe protocol data"),
CI must inspect the **actual workspace graph** — `cargo metadata`, resolved
edges — or compile an external-consumer falsifier. It must not require a crate
to self-enrol for the check to see it, which is exactly the failure mode
`nmp-audit` shipped with.

That check is **not built**. It is tracked separately from this deletion; do not
read §4.1 as claiming a mechanical guarantee. What §4.1 claims today is that the
*bad* mechanism is gone and that ownership is described by the real dependency
shape, which is auditable by reading the graph even before CI does it
automatically.

### 4.4 Scope-drift prevention

- A crate reading or writing a kind whose schema it does not define does not
  thereby gain any routing authority over it — there is no authority to gain.
  It may add only its typed, per-intent contextual contribution (§3.2.1).
- Drift into ownership of a foreign schema requires copying that schema's
  parser/builder into a second crate. That is a visible, reviewable diff, not a
  silent table entry.

---

## 5. Modularity — where each piece lives

| Piece | Crate |
|---|---|
| Default policy (§2), lanes, solver, coalescing, route compilation | `nmp-router` (types + compile) / `nmp-engine` (write-path execution) — **core knows zero NIPs beyond NIP-01/65 defaults** |
| Each NIP's schema: build, parse, validate, typed values, demand constructors | that NIP's own `nmp-nip*` / `nmp-blossom` crate, engine-free |
| The destination and body of a supported write | the layer that mints that write, operation-locally (§3.1) |
| NIP-17 (future): gift-wrap machinery, kind:10050 ingestion, and its own fail-closed private-inbox resolution | `nmp-mod-nip17` (future) — its routing is owned by the operation that publishes a gift-wrap, not by a registry entry for kind 1059 |
| NIP-29: the NIP-29-defined management/state schemas. Group-bound foreign drafts use §3.2.1 to add `h` + host context without defining the foreign schema. | `nmp-nip29` |

An app that enables nip17 links DM routing; an app that doesn't links **zero** DM code — because the code is in a crate it did not depend on, and core contains no string "gift wrap" anywhere. The seam already half-exists: `Lane::GroupHost`/`Lane::DmInbox` and `pinned_relays()` were built as module-fed facts; what feeds them is a typed operation, not a global policy table.

---

## 6. BUILT vs NEW

**BUILT (extend, don't touch semantics):** lane-typed facts + `RelayDirectory` + `LiveDirectory` self-bootstrap; `DiscoveryKinds` incl. full 1xxxx range; additive relay roles; greedy capped 2-min solver + `Shortfall`; widen-only coalescing + delivery re-filter; skeleton-stable sub-ids + surgical diffing; read-side `RouteProvenance`; `sync_discovery` widen-only wave-2; write outbox stages + `NarrowOnly`/`PrivateNarrow`; per-relay diagnostics.

**NEW:**
1. `Lane::{AppRelay, Fallback, Nip65Read}`; `RelayDirectory::{app_relays, fallback_relays, read_relays, ingest_read_relays}`; config surface for the three lanes.
2. Solver-input narrowing (own relays only toward k) + additive lane application outside the solve + shortfall-triggered fallback with appRelay suppression.
3. Default write policy derived from the event (`WriteRouting::Default`): p-tag inbox fan-out (read-marked, 2-each), the >10-p-tag / kind:3 / kind:1xxxx exclusions, app-lane union; delete the flagged `ToInboxes` write-relay fallback.
4. ~~Route-policy override primitive and claim-table routing.~~ **CANCELLED
   (#859; #757/#758 closed NOT_PLANNED.)** Nothing replaces it — see §3.1.
5. ~~`RouteClass` threaded through `WriteStatus::Routed`.~~ **CANCELLED (#859)** —
   see §3.3.
6. ~~`KindClaim`/`KindScope`/`ModuleRegistration` + the `nmp-audit` workspace
   test.~~ **DELETED (#859).** `crates/nmp-ownership` and `crates/nmp-audit` are
   gone, along with every `claims()` export. The replacement obligation is the
   workspace-graph dependency check in §4.3, which is TARGET.
7. The §2.6 scenarios as router/engine tests; a **decision-table test** covering
   every (lane config × fail-mode × p-tag/kind exclusion) cell.

Rough order: 1–3 are M-next (they close a known-gap and finish Part A); 7 rides
with them. Items 4–6 are not scheduled because they are cancelled.

---

## 7. Biggest risk

**The lane-composition matrix.** Lanes × fail modes × pre-signed × p-tag
exclusions is a genuine decision *table*, and every cell is a routing decision
where one wrong precedence is either a privacy leak or silent under-delivery
(fallback suppressed when it shouldn't be, an under-min author quietly
unrouted). Deleting the override primitive (§3) removes the *policy* dimension
from that matrix — a real reduction in the space that can rot — but the
remaining cells still need the full decision table enumerated as tests (§6 item
7), plus ledger entries whose falsification attempts target exactly the leak
cells. Secondary risk: §6 item 2 changes the meaning of an already-proven
solver's input — re-run every solver/coverage-attribution property test against
the narrowed candidate semantics before trusting any plan diff.

**The risk this document created.** For a year Parts B/C described a mechanism
that did not exist and, in the case of `nmp-audit`, one that existed and proved
nothing. That is the more expensive failure mode: a design doc asserting a
guarantee is read as the guarantee. Status lines in this file are load-bearing;
keep them honest or delete the section.

## 8. Decisions that still need the owner

1. **Discovery-kind self-writes to indexers** (§2.1): does the author's own kind:10002/kind:0 publish also go to the configured indexers by default? Spec assumes **yes** (bootstrap symmetry); confirm.
2. **Pre-signed sensitive kinds without their module linked:** an app publishing a pre-signed kind:1059 with no nip17 module gets ordinary default routing. Alternatives: (a) accept (current spec — modularity-pure: core cannot know 1059 is sensitive), (b) a tiny core-shipped "known-sensitive, refuse-unowned" kind set (violates zero-NIP-knowledge), (c) refuse ALL pre-signed publishes of any 1xxx-gift-wrap-range kind. Spec recommends (a) + loud documentation; owner may prefer (b). Resolved as (a) in §9.2 — the resolution stands, but note that its phrasing predates #859 and the mechanism is "the composing crate owns the write", not a registry entry.
3. **Do `Hint`/`Provenance` extras count toward the 2-relay-min**, or only `Nip65Write`? Owner said "author's own relays (from kind:10002)"; spec reads that literally (extras are additive candidates but don't satisfy the min ⇒ under-min authors still trigger fallback). Confirm — the other reading (hints count) reduces fallback traffic. (Resolved in §9.3.)
4. ~~**`Manual`/`Imported` route-class surface.**~~ **MOOT (#859)** — `RouteClass` is deleted; there are no route classes to gate.
5. **Draft-relay list kind** for a future drafts module (kind:10013 per NIP-37?) — pick when the module is built. It will be that module's own routing fact, not a registered relay source.

## 9. Owner decisions RESOLVED (2026-07-11)

1. **Discovery self-writes to indexers: YES** — and add **indexer backfill**: when NMP receives a *newer* event (e.g. a fresher kind:0/10002) that the indexer it's using did not have, it contributes that event *back* to that indexer (republish), keeping the indexer fresh. New write-back behavior for the router milestone.
2. **Pre-signed sensitive kinds without their module linked: accept (a)** — core stays NIP-blind. Raw **kind:1059 (gift-wrap) has no mandated routing in core**; routing is owned by whatever crate *composes over* gift-wraps. `nip17` (and any other gift-wrap-composing module) does its **own** routing (recipient's kind:10050, fail-closed) inside the operation that mints the write. If no such module is linked, a bare pre-signed 1059 routes by the default policy — which is *why* linking the composing module is what makes it safe. *(2026-07-27: the decision stands; the "`RoutePolicy`/ownership seam" it originally named is deleted per #859, and the seam is now the write-minting operation itself — §3.1.)*
3. **Relay hints count toward the 2-min (BOTH read+write), and NMP EMITS hints on publish.** Two additions: (a) on READ, a relay hint (from an `e`/`p` tag's 3rd position, or from provenance of where an event was seen) is a first-class routing candidate that *counts* toward the 2-relay-min — favor following hints. (b) on WRITE, when publishing an event that references another event/user, NMP writes a **relay hint = the relay where it found the referenced event** into that tag. This supersedes §8-decision-3's literal "only write relays count" reading.
4. ~~**`Manual`/`Imported` route classes: feature-gated, tooling-only.**~~ **MOOT (#859)** — the `RouteClass` enum this decided is deleted.
5. **Drafts: kind:10013 (NIP-37), but do NOT build the module yet** — capture the known requirements in a follow-up GitHub issue; spec it high-level only.
6. **Schema ownership and contextual contribution are distinct.** NIP-29 owns
   only its exact NIP-defined event schemas. A group-bound publication of a
   foreign-owned draft adds the correct `h` tag and group-host route as typed
   per-intent context before the core signs once; NIP-29 does not claim that
   draft's kind.

**Adjacent correction (NOT part of this spec — network layer):** signature verification is **kind-independent** and belongs in `nmp-transport` (the network boundary), verified **once per event id** (redeliveries only string-compare the signature, never re-schnorr); an invalid signature is an **evil-relay** signal (drop + flag relay health), never a per-kind concern; cache reads are never re-verified. Being fixed in the hardening pass, not here.
