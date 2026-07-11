# Relay routing + kind ownership — the canonical spec

- **Date:** 2026-07-11
- **Status:** Owner-confirmed default policy (Part A); designed override primitive + provenance decision (Part B); designed ownership boundary (Part C). Parts B/C carry one explicit owner-decision list (§8). Provisional-until-v2 like everything else, but Part A is the owner's settled routing model — do not re-litigate its rules, only their mechanics.
- **2026-07-11 promotion correction:** ownership below is **schema ownership**, not
  ownership of every event that participates in a protocol context. A NIP module
  claims only the exact event schemas that NIP defines. Per-publication context
  contributed to a foreign-owned unsigned draft is a separate, typed operation
  (§3.2.1); it never expands the module's `KindClaim`.
  Likewise, historical "no `relays:` parameter" wording means no untyped route
  override on the default path; a live query may carry explicit typed source
  authority as specified in `query-demand-and-evidence.md`.
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
| Widen-only coalescing (`AuthorUnion`, `KindUnion`; unproven rules dropped, ship separate) + local re-filter on delivery | `coalesce.rs`, `deliver.rs` |
| Per-relay REQ partitioning, skeleton-stable `SubId` (author churn = one overwriting REQ), full-recompile-then-surgical-diff | `plan.rs`, `router.rs` |
| Read-side typed provenance: every `WireReq` carries `Vec<RouteProvenance>` (relay, lane, covered authors, `OutboxSolved`\|`Pinned`) — no wire REQ without a traceable route | `route.rs::RouteProvenance` |
| Self-bootstrapping outbox: `sync_discovery` opens a widen-only internal kind:10002 sub against indexers for authors with unknown write relays (wave 2 supersession) | `core/mod.rs::sync_discovery` |
| Write outbox: durable `WriteIntent` → `WriteStatus` stream; `WriteRouting::{AuthorOutbox, ToInboxes, PrivateNarrow}`; `NarrowOnly<T>` (no widen op — ledger #6 fail-closed as a type) | `outbox/mod.rs`, `core/mod.rs::resolve_routes` |
| Diagnostics: per-relay sub counts, by-lane counts, reverse coverage, exact filters, uncovered authors, dropped rules | `diag.rs` |

Known deviations this spec resolves: `ToInboxes` falls back to recipients' *write* relays (flagged inline as NOT correct); the solver counts indexer/extra candidates toward the 2-min; there is no app-relay or fallback-relay concept; write routing is caller-chosen rather than policy-derived; no per-kind override seam; no kind-ownership boundary.

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

The default write route is **derived from the event**, not chosen by the caller (see §2.5). Union of:

1. **Author's WRITE-marked relays** (kind:10002), all of them (a write fans out to every known write relay; no coverage solve — BUILT in `resolve_routes::AuthorOutbox`), 2-min semantics only in the fallback trigger sense below.
2. **p-tagged recipients' INBOX relays** — each recipient's **READ-marked** kind:10002 relays (2 each, same top-up trigger), **unless** any of:
   - E has **more than 10 p-tags** (11+ ⇒ skip inbox fan-out entirely; **exactly 10 still publishes** — this is a broadcast-spam guard, not a mention limit),
   - E is **kind:3** (a contact list p-tags everyone you follow; it is not addressed *to* them),
   - E is **kind:1xxxx** (10000–19999; replaceable lists p-tag their members, same non-addressed semantics).
3. **appRelay** — always, additive.
4. **fallbackRelay** — for the *author* side when the author's own write relays number `< 2`, iff no appRelay. (Recipient-side under-min top-up is NOT done from the sender's lanes — you don't get to spray someone else's inbox to your fallback relay; an under-min recipient simply gets fewer inbox copies. This is the same trust rule as Part B's fail modes.)

### 2.4 The READ-marked / WRITE-marked kind:10002 distinction (closes the `ToInboxes` gap)

The distinction now matters on both sides: reads consume authors' **write**-marked relays; the write path's p-tag inbox fan-out consumes recipients' **read**-marked relays. Mechanics:

- `parse_nip65_write_relays` (engine) already drops `"read"`-marked entries. Add `parse_nip65_read_relays` (unmarked = both, per NIP-65).
- `RelayDirectory` grows `read_relays(&PubkeyHex) -> Vec<LanedRelay>` (lane: new `Nip65Read`) and `ingest_read_relays` (additive default no-op, mirroring `ingest_write_relays`); `LiveDirectory` stores both sets from the same kind:10002 winner in one `ingest` pass.
- `resolve_routes`' inline-flagged deviation (union of recipients' *write*+extra relays) is deleted, replaced by `read_relays` — recipient discovery for unknown recipients rides the existing `sync_discovery` machinery (kind:10002 is one event covering both sides; no second discovery sub).

### 2.5 Who picks `WriteRouting`

Today the caller supplies `WriteRouting` on the intent. Under this spec the default is **`WriteRouting::Default`** (new variant): the engine derives §2.3's union from the signed event itself at route-resolution time (kind, author, p-tags are all on the event). `AuthorOutbox`/`ToInboxes` remain as internals the default resolves *into*; `PrivateNarrow` remains the fail-closed narrow type, now minted only by route policies (Part B) — the app-facing publish surface takes an event + durability, nothing more. This is what makes "no `relays:` param" true on the write side too.

### 2.6 Acceptance scenarios (these are the tests)

**A1 — kind:30023 feed.** appRelay = `my-app-relay`. u1 writes to {relay1, relay2}, u2 to {relay2, relay4}, u3 to {relay1, relay4}. Query `kinds:[30023], authors:[u1,u2,u3]` compiles to exactly: `my-app-relay` ← all three authors (app lane, additive); `relay1` ← [u1,u3]; `relay2` ← [u1,u2]; `relay4` ← [u2,u3] (coverage-solved, AuthorUnion-coalesced, one REQ per relay). No fallback fires (appRelay suppresses; all authors at 2). Diagnostics show `by_lane` counting `AppRelay` separately from `Nip65Write`.

**A2 — kind:0 two-wave reactive flow.** Query `kinds:[0], authors:[uX]`, uX unknown. Wave 1: kind:0 is a discovery kind ⇒ routes immediately to indexers (+appRelay); a possibly-stale kind:0 renders. In parallel `sync_discovery` widens its kind:10002 sub with uX. Wave 2: uX's 10002 lands ⇒ same recompile routes the kind:0 atom additionally to uX's own write relays (skeleton-stable sub-id ⇒ overwriting REQ, no churn); the store's replaceable supersession makes the fresher kind:0 the winner. Nothing is torn down; the app saw one live query throughout.

**A3 — p-tag publish.** appRelay = `my-app-relay`. u1 (writes to {r1, r2}) publishes kind:1 p-tagging u2 (read-marked inbox {r2, r4}). Route = {`my-app-relay`, r1, r2, r4} — r2 appears once with both roles' provenance (author-write ∪ recipient-inbox; additive roles, same as the read side). The same event with 11 p-tags routes to {`my-app-relay`, r1, r2}; with exactly 10, inboxes still fan out. The same event as kind:3 routes to {`my-app-relay`, r1, r2} regardless of p-tag count.

---

## 3. Part B — The `RoutePolicy` override primitive

A protocol module that **owns** certain event schemas (Part C) may replace the
default policy for exactly those schemas. The router stays NIP-agnostic the same
way it already is for `GroupHost`/`DmInbox` pins: **the module feeds classified
facts and declares a policy value; the router only ever executes
closed-vocabulary values** (VISION §10: values in, code after — a policy is data,
never a closure).

### 3.1 Shape

```rust
/// Supplied by a module for the kinds it OWNS (audited, §4). One policy
/// covers reads AND writes for those kinds.
pub struct RoutePolicy {
    /// Where relays come from when reading these kinds.
    pub read_source: RelaySource,
    /// Where relays come from when writing these kinds.
    pub write_source: RelaySource,
    /// Whether the app lanes (appRelay/fallbackRelay) still apply.
    /// Default policy = Apply; an override defaults to Skip for BOTH
    /// read and write (owner-confirmed).
    pub app_lanes: AppLanes,        // Apply | Skip
    /// What happens when the source resolves to ZERO relays.
    pub on_empty: FailMode,         // Closed | OpenToAppLanes
    /// The typed provenance every wire route/publish under this policy
    /// carries (§3.3). No default.
    pub route_class: RouteClass,
}

/// CLOSED vocabulary — extend the enum, never admit module code.
pub enum RelaySource {
    /// The default: author's write-marked 10002 (reads/author-writes)
    /// + p-tag recipients' read-marked 10002 (writes), §2.
    Nip65Default,
    /// A per-pubkey replaceable relay-list kind other than 10002
    /// (NIP-17 → kind:10050; drafts → the user's draft-relay list kind).
    /// Discovery rides sync_discovery unchanged: every 1xxxx kind is
    /// already a DiscoveryKind, so the engine can bootstrap these lists
    /// from indexers with zero new machinery.
    RelayListKind { kind: u16 },
    /// Pinned facts the owning module ingested into the directory under
    /// a named lane (NIP-29 → group-anchor relays derived from group
    /// state, lane GroupHost). The router reads pinned_relays(); it
    /// never knows what a "group" is.
    PinnedLane(Lane),
}
```

- **How a module supplies it:** as part of its `KindClaim` (§4.1) — ownership and routing authority are one declaration, registered at engine construction. The router holds a `BTreeMap<u16, RoutePolicy>` **derived from the claim table** (table-driven; there is no hand-maintained per-kind `match` anywhere — the legacy per-kind match is explicitly left behind).
- **How the router applies it:** atom/event classification happens once, up front: an atom whose `kinds` are all owned by one policy routes under that policy; the default policy otherwise. A filter mixing policy-owned and default kinds is **split** into per-policy atoms before routing (same machinery as skeleton grouping — kinds are already a set field; widen-only coalescing may re-merge per relay afterwards where the policies agree). An event's kind picks the write policy the same way.
- **App-lane skip:** `AppLanes::Skip` removes appRelay/fallbackRelay from both directions for those kinds. A NIP-17 gift-wrap never touches the app relay even though the app relay takes "everything" — *everything* means everything default-routed.
- **Fail modes — the trust rule:** *someone else's private inbox → fail closed; your own recoverable content → fail open.*
  - **NIP-17 fails CLOSED:** recipient has no kind:10050 ⇒ the write terminates `WriteStatus::Failed` before any `PublishEvent` effect exists (structurally: the policy mints a `PrivateNarrow`/`NarrowOnly` set; empty ⇒ the existing ledger-#6 mechanism fires). It never falls to appRelay, even when one is configured. Reads simply have no route (coverage stays `Unknown` — ledger #7 keeps that honest).
  - **Drafts fail OPEN:** no draft-relay list ⇒ `OpenToAppLanes` re-admits appRelay/fallbackRelay *for this event only*. Your own recoverable content on your own app relay is fine.
- **`Skip` + `OpenToAppLanes` compose:** skip means "app lanes are not additive routes"; open-on-empty means "app lanes are the last-resort route when the source is empty." These are different questions and both fields are required — no defaulting.

### 3.2 What this is NOT

- Not a raw `relays:` parameter — policies attach to *kinds via ownership*, never
  to a query or an intent. Typed query source authority and typed per-intent
  context are separate closed values; neither is an unclassified override.
- Not a closure/callback — `RelaySource` is closed; a module needing a new source shape extends the enum through review (exactly the Selector-vocabulary rule).
- Not a second routing engine — policies swap the *fact source* and *lane applicability*; coverage solving, coalescing, sub-id stability, diffing, delivery re-filter all run unchanged on the policy's relays.

### 3.2.1 Contextual publication is not kind ownership

Some protocols constrain an event without defining its schema. NIP-29 is the
forcing example: a group-bound publication may carry an event whose kind is
owned by another module (or is unowned), while NIP-29 contributes the required
`h` tag and the group's host relay.

That operation is **per unsigned draft**, not a `RoutePolicy` for the draft's
kind:

1. The schema owner builds and validates an immutable unsigned draft.
2. The contextual module returns a new draft containing only its protocol
   contribution, plus typed route context scoped to that publication.
3. The core validates the composed draft, selects the default signer or an
   explicit signer override, signs exactly once, and publishes through the
   ordinary outbox and receipt machinery.

The contextual module cannot rewrite schema-owned fields, claim the foreign
kind, or install a kind-wide route override. A pre-signed event is immutable and
therefore cannot acquire missing group context; it can only be published
verbatim. NIP-29's own management/state event schemas still use ordinary
`KindClaim` + `RoutePolicy` routing.

### 3.3 Typed route provenance — DECISION: adopt (`RouteClass`, no default)

The owner deferred this; the call here is **adopt**, and the reasoning is short because half of it is already in the tree:

1. **The read side already has it.** `RouteProvenance{lane, route_kind}` means every wire REQ traces to a typed reason today. Declining typed classes on the write side would make writes the *less* accountable direction — backwards, given writes are where leaks happen (ledger #6).
2. **Ledger #6's mechanism needs it to generalize.** `NarrowOnly` proves fail-closed for one variant; `RouteClass` makes "which trust regime routed this publish" a required, inspectable fact on every publish, and lets the ownership gate (§4.3) be one table lookup instead of per-kind knowledge.
3. **Fail-closed-by-construction is free.** `enum RouteClass` with **no `Default` impl and no anonymous constructor**: every publish carries exactly one, minted by the policy engine — an unclassifiable publish cannot be represented, so it cannot reach the wire. This is a type mechanism, admissible under the ledger's own standard (lints are not; types are).

```rust
/// Why this publish is going where it's going. NO Default. Minted by the
/// routing layer only; apps never construct one.
pub enum RouteClass {
    /// Default policy (§2): author outbox + p-tag inboxes + app lanes.
    Automatic,
    /// A module-pinned host (NIP-29 group anchor).
    HostPinned,
    /// A verified private inbox route (NIP-17 kind:10050 resolution) —
    /// carries the NarrowOnly set; only narrowing exists.
    VerifiedPrivateInbox,
    /// Explicit tooling route (nmp-demo / diagnostics CLI). Feature-gated
    /// out of the app-facing SDK build — see §8 (owner decision 4).
    Manual,
    /// Re-broadcast of an event authored elsewhere (import/mirror tools).
    Imported,
    /// Diagnostic probes (capability probing, NIP-66-style checks).
    Diagnostic,
}
```

**Pre-signed events fail closed.** `WritePayload::Signed` (already-signed verbatim publish, the M4 gap in known-gaps) is exactly where a leak sneaks in — no signing step, no template inspection. Rule: a pre-signed publish is routed **only** by the claim table: owned kind ⇒ the owner's policy (and its class); unowned kind ⇒ default policy ⇒ `Automatic`. `Manual`/`Imported` are the only classes an operator surface can request explicitly, and they are not in the app SDK build. A pre-signed kind:1059 with no nip17 module linked therefore has an owner-less sensitive kind and routes `Automatic` — which is why nip17's claim exists and why linking it is what makes DMs *safe*, not just possible (see §5 modularity and §8 decision 2).

`WriteStatus::Routed` grows the class (`Routed(RouteClass, BTreeSet<RelayUrl>)`) so receipts and diagnostics show the trust regime, not just the URLs.

---

## 4. Part C — The kind-ownership boundary

The legacy repo proved the *essence*: positive typed claims, a static workspace audit as the load-bearing enforcement, a runtime publish gate, ownership gating route authority. It also proved what to drop: the linker-symbol collision gimmick, the per-kind `match`, per-kind claim repetition, regex source scans, and doctrine-lints (inadmissible here by the ledger's own falsification standard — every mechanism below is a type or a data-driven CI test over declared values, not a source-text scan).

### 4.1 The claim — one typed fact per module

```rust
/// Declared by a protocol module, const/static data. Registered at engine
/// construction; collected by the workspace audit.
pub struct KindClaim {
    pub owner: ModuleId,                  // e.g. "nip17", "nip29", "drafts"
    pub scope: KindScope,
    /// Exclusive: no other module may claim an overlapping scope, and the
    /// publish gate (§4.3) applies. Non-exclusive claims exist for shared
    /// mechanisms (none known yet; the variant exists so the audit can
    /// distinguish deliberate sharing from drift).
    pub exclusive: bool,
    /// Routing authority: present iff this module overrides routing for
    /// this scope. A RoutePolicy is ONLY accepted attached to a claim —
    /// this is the gate: no ownership, no route override.
    pub route_policy: Option<RoutePolicy>,
}

pub enum KindScope {
    Kind(u16),
    Range(RangeInclusive<u16>),           // only when a NIP truly owns the range
    Set(&'static [u16]),                  // NIP-17's {1059, 13, 14, 15, 10050}
}
```

Range/Set kill the legacy per-kind repetition. A module exports `pub fn claims() -> &'static [KindClaim]` — plain data, no macro magic required (`declare_crate_ownership!` may return as sugar later; it is not the mechanism).

### 4.2 Enforcement — three layers, none of them a lint

1. **Engine-construction check (types + fail-fast).** The engine builder takes `Vec<ModuleRegistration>`; construction folds all claims into the routing table and **errors** (typed, not panic) on any exclusive-scope overlap among *linked* modules. Cheap, runs in every test that builds an engine, catches the common case at the first `cargo test`.
2. **The static workspace audit (the load-bearing layer).** A CI test (`nmp-audit` dev-crate or a workspace-level integration test) that dev-depends on **every** module crate — enumerated from `cargo metadata`, so a new module crate that forgets to enroll is itself a red build — collects all `claims()`, and asserts: (a) no exclusive-scope overlap **across the whole workspace, including modules no app currently links** (the legacy audit's key property: drift is caught before any consumer collides); (b) every `RoutePolicy` is attached to a claim whose scope covers it (route authority ⊆ ownership, by construction of the struct, re-asserted across crates); (c) claimed scopes don't intersect `DiscoveryKinds` unless flagged (a module claiming 1xxxx must consciously interact with indexer semantics). Output on failure names both owners and the overlapping scope — the legacy `NMP-OWNERSHIP-COLLISION` map, minus the linker.
3. **The runtime publish gate (table-driven).** At route resolution, one lookup: event kind → claim table. Owned-exclusive kind ⇒ routed by the owner's policy, carrying the owner's `RouteClass`; the default policy **refuses** to route it (structurally: the routing table returns the policy, and `Automatic` is simply not the entry for that kind — there is no bypass parameter to pass). Unowned kind ⇒ default policy, `Automatic`. This replaces the legacy `validate_publish_ownership` per-kind provenance checks with the class system: "kind 1059 requires nip17 provenance" becomes "kind 1059's table entry routes `VerifiedPrivateInbox` or fails closed" — same guarantee, zero per-kind code in core.

**Dropped from legacy, deliberately:** the compile-time linker-symbol collision (fragile across feature unification/platforms, redundant once layer 2 exists and layer 1 fails fast); the source-surface regex scan (a lint by another name); the hand-maintained per-kind `match` (the table *is* the claims).

### 4.3 Ownership gates routing authority

The rule, stated once: **a module's schema-wide `RoutePolicy` is honored for
exactly the kinds it owns.** The `Option<RoutePolicy>` living *inside*
`KindClaim` makes this true by construction — there is no standalone "register a
route policy" API to misuse. A typed contextual publication (§3.2.1) is the only
separate case: it contributes route context to one composed intent and cannot
be registered as policy for the foreign kind. The two legacy facts ("owns
kind" + "overrides route") remain independently *checkable* but cannot drift
into a module claiming unrelated content schemas.

### 4.4 Scope-drift prevention

- A module reading/writing a kind outside its claim doesn't get a schema-wide
  policy for it — it gets the default policy like any app code. A contextual
  operation may add only its typed, per-intent contribution (§3.2.1). Drift
  therefore cannot silently turn into ownership of the foreign schema.
- The audit's cross-workspace overlap check means a second module claiming an owned kind is a red build even if no app links both.
- New ledger entry (proposed **#14 — Route-override without ownership / ownership collision**): "a routing override is only representable inside a `KindClaim`; overlapping exclusive claims are a red build workspace-wide; an owned kind cannot be routed by the default policy." Falsification: attempt to publish an owned kind through `Automatic`, attempt to register a bare policy, attempt two claims on one kind — each must fail to compile or fail closed.

---

## 5. Modularity — where each piece lives

| Piece | Crate |
|---|---|
| Default policy (§2), lanes, solver, coalescing, `RoutePolicy`/`RelaySource`/`RouteClass`/`KindClaim` **types**, claim-table routing, publish gate | `nmp-router` (types + compile) / `nmp-engine` (write-path execution) — **core knows zero NIPs beyond NIP-01/65 defaults** |
| Ownership audit harness | workspace-level dev-crate (`nmp-audit`), cargo-metadata-driven |
| NIP-17: claim `{1059,13,14,15,10050}`, policy `{read/write: RelayListKind{10050}, Skip, Closed, VerifiedPrivateInbox}`, kind:10050 ingestion, gift-wrap machinery | `nmp-mod-nip17` (future) |
| NIP-29: claim the explicit set of NIP-29-defined management/state schemas only (no broad ranges and no foreign content kinds); those owned schemas may use `{PinnedLane(GroupHost), Skip, Closed, HostPinned}`. Group-bound foreign drafts use §3.2.1 to add `h` + host context without changing ownership. | `nmp-mod-nip29` (future) |
| Drafts: claim (draft kind), policy `{RelayListKind{draft-list-kind}, Skip, OpenToAppLanes, Automatic}` | `nmp-mod-drafts` (future) |

An app that enables nip17 links DM routing; an app that doesn't links **zero** DM code — the claim table simply has no 1059 entry, and core contains no string "gift wrap" anywhere. The seam already half-exists: `Lane::GroupHost`/`Lane::DmInbox` and `pinned_relays()` were built as exactly this kind of module-fed fact; Part B/C give them their supplier.

---

## 6. BUILT vs NEW

**BUILT (extend, don't touch semantics):** lane-typed facts + `RelayDirectory` + `LiveDirectory` self-bootstrap; `DiscoveryKinds` incl. full 1xxxx range; additive relay roles; greedy capped 2-min solver + `Shortfall`; widen-only coalescing + delivery re-filter; skeleton-stable sub-ids + surgical diffing; read-side `RouteProvenance`; `sync_discovery` widen-only wave-2; write outbox stages + `NarrowOnly`/`PrivateNarrow`; per-relay diagnostics.

**NEW:**
1. `Lane::{AppRelay, Fallback, Nip65Read}`; `RelayDirectory::{app_relays, fallback_relays, read_relays, ingest_read_relays}`; config surface for the three lanes.
2. Solver-input narrowing (own relays only toward k) + additive lane application outside the solve + shortfall-triggered fallback with appRelay suppression.
3. Default write policy derived from the event (`WriteRouting::Default`): p-tag inbox fan-out (read-marked, 2-each), the >10-p-tag / kind:3 / kind:1xxxx exclusions, app-lane union; delete the flagged `ToInboxes` write-relay fallback.
4. `RoutePolicy` + `RelaySource` + claim-table routing (read-atom splitting by policy, write-kind dispatch), `AppLanes`/`FailMode` composition.
5. `RouteClass` (no default) threaded through `WriteStatus::Routed` + diagnostics; pre-signed publish routed only via the table.
6. `KindClaim`/`KindScope`/`ModuleRegistration`, construction-time overlap error, the `nmp-audit` workspace test, ledger #14 + falsification tests.
7. The §2.6 scenarios as router/engine tests; a **decision-table test** covering every (lane config × policy × fail-mode × p-tag/kind exclusion) cell.

Rough order: 1–3 are M-next (they close a known-gap and finish Part A); 4–5 land with the first real module need (nip17 is the forcing function); 6 lands the moment a second claim-bearing module exists — the audit is cheap and should not wait for a collision.

---

## 7. Biggest risk

**The policy-composition matrix.** Lanes × overrides × fail modes × pre-signed × p-tag exclusions is a genuine decision *table*, and every cell is a routing decision where one wrong precedence is either a privacy leak (a gift-wrap reaching appRelay because `Skip` and `OpenToAppLanes` were composed wrong, or a pre-signed 1059 sliding through `Automatic` in an app that forgot to link nip17) or silent under-delivery (fallback suppressed when it shouldn't be, an under-min author quietly unrouted). The individual mechanisms are each simple; the *interactions* are where this design can rot. Mitigation is stated in §6 item 7 and is non-negotiable: the full decision table enumerated as tests, plus ledger entries whose falsification attempts target exactly the leak cells. Secondary risk: §6 item 2 changes the meaning of an already-proven solver's input — re-run every solver/coverage-attribution property test against the narrowed candidate semantics before trusting any plan diff.

## 8. Decisions that still need the owner

1. **Discovery-kind self-writes to indexers** (§2.1): does the author's own kind:10002/kind:0 publish also go to the configured indexers by default? Spec assumes **yes** (bootstrap symmetry); confirm.
2. **Pre-signed sensitive kinds without their module linked** (§3.3): an app publishing a pre-signed kind:1059 with no nip17 module gets `Automatic` public routing. Alternatives: (a) accept (current spec — modularity-pure: core cannot know 1059 is sensitive), (b) a tiny core-shipped "known-sensitive, refuse-unowned" kind set (violates zero-NIP-knowledge), (c) refuse ALL pre-signed publishes of any 1xxx-gift-wrap-range kind. Spec recommends (a) + loud documentation; owner may prefer (b).
3. **Do `Hint`/`Provenance` extras count toward the 2-relay-min**, or only `Nip65Write`? Owner said "author's own relays (from kind:10002)"; spec reads that literally (extras are additive candidates but don't satisfy the min ⇒ under-min authors still trigger fallback). Confirm — the other reading (hints count) reduces fallback traffic.
4. **`Manual`/`Imported` surface** (§3.3): confirmed as feature-gated tooling-only (absent from the app SDK), or removed entirely until a tool needs them?
5. **Draft-relay list kind** for the drafts module's `RelayListKind` source (kind:10013 per NIP-37?) — pick when the module is built.

## 9. Owner decisions RESOLVED (2026-07-11)

1. **Discovery self-writes to indexers: YES** — and add **indexer backfill**: when NMP receives a *newer* event (e.g. a fresher kind:0/10002) that the indexer it's using did not have, it contributes that event *back* to that indexer (republish), keeping the indexer fresh. New write-back behavior for the router milestone.
2. **Pre-signed sensitive kinds without their module linked: accept (a)** — core stays NIP-blind. Raw **kind:1059 (gift-wrap) has no mandated routing in core**; routing is owned by whatever crate *composes over* gift-wraps. `nip17` (and any other gift-wrap-composing module) does its **own** routing (recipient's kind:10050, fail-closed) via the `RoutePolicy`/ownership seam. If no such module is linked, a bare pre-signed 1059 routes `Automatic` — which is *why* linking the composing module is what makes it safe.
3. **Relay hints count toward the 2-min (BOTH read+write), and NMP EMITS hints on publish.** Two additions: (a) on READ, a relay hint (from an `e`/`p` tag's 3rd position, or from provenance of where an event was seen) is a first-class routing candidate that *counts* toward the 2-relay-min — favor following hints. (b) on WRITE, when publishing an event that references another event/user, NMP writes a **relay hint = the relay where it found the referenced event** into that tag. This supersedes §8-decision-3's literal "only write relays count" reading.
4. **`Manual`/`Imported` route classes: feature-gated, tooling-only** (absent from the app SDK build) — orchestrator's call, per the spec recommendation.
5. **Drafts: kind:10013 (NIP-37), but do NOT build the module yet** — capture the known requirements in a follow-up GitHub issue; spec it high-level only.
6. **Schema ownership and contextual contribution are distinct.** NIP-29 owns
   only its exact NIP-defined event schemas. A group-bound publication of a
   foreign-owned draft adds the correct `h` tag and group-host route as typed
   per-intent context before the core signs once; NIP-29 does not claim that
   draft's kind.

**Adjacent correction (NOT part of this spec — network layer):** signature verification is **kind-independent** and belongs in `nmp-transport` (the network boundary), verified **once per event id** (redeliveries only string-compare the signature, never re-schnorr); an invalid signature is an **evil-relay** signal (drop + flag relay health), never a per-kind concern; cache reads are never re-verified. Being fixed in the hardening pass, not here.
