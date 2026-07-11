# Routing Unit E — the design note (RoutePolicy + RelaySource + claim-table routing)

- **Date:** 2026-07-11
- **Status:** Design note (pre-build), the load-bearing back-half of milestone
  MR. Fans out to builders. Writes no code.
- **Promotion correction:** the landed `nmp-ownership` facts below remain
  authoritative. The unbuilt Unit E plan applies only to schema-wide policy for
  exact claimed schemas. It is superseded wherever it implies that a contextual
  protocol owns a foreign event kind; §1.5 records the corrected boundary.
- **Source of truth (do not re-litigate):** `docs/design/routing-and-ownership.md`
  Parts B/C + §9 RESOLVED decisions; `docs/design/routing-build-plan.md` §3
  (Unit E), §7.1 resolutions (Q6, Q7). Where those are silent, §5 below
  surfaces an owner question — it invents no resolutions.
- **Ground truth read for this note:** the LANDED `nmp-ownership` crate
  (`crates/nmp-ownership/src/{lib,route_class,route_policy,relay_source,kind_claim,kind_scope,module_id}.rs`,
  epic #35); `crates/nmp-router/src/{router,route,facts,lib}.rs`;
  `crates/nmp-engine/src/core/mod.rs` (`sync_discovery`, `resolve_routes`,
  `EngineCore::new`); `crates/nmp-engine/src/outbox/mod.rs`.

## 0. What is already on the ground (so E extends, never re-plans)

- **Units A and B have LANDED.** `Lane::{AppRelay, Fallback, Nip65Read}`
  (`facts.rs:40,44,37`), the additive-lane routes applied OUTSIDE the solve
  (`route.rs::{indexer_lane_routes,app_lane_routes,fallback_lane_routes}`,
  `router.rs::compile:121-155`), `RelayDirectory::{app_relays,fallback_relays,
  read_relays,ingest_read_relays}` (defaulted, `facts.rs:84-118`),
  `LiveDirectoryBuilder` (`facts.rs:424`), and `parse_nip65_read_relays`
  (`core/mod.rs:1489`) are all in the tree and tested. E builds on this, does
  not touch it.
- **Unit C has NOT landed.** `resolve_routes` (`core/mod.rs:658`) still has the
  three-arm `AuthorOutbox | ToInboxes | PrivateNarrow` match with the flagged
  `ToInboxes` write-relay-fallback deviation (`:648-657`, `:678-700`); there is
  no `WriteRouting::Default` (`outbox/mod.rs:50`). **E's write-side dispatch
  (E2) depends on C.** E's read-side (E1) does not.
- **`nmp-ownership` (#35) is LANDED and is types-only.** Critically:
  `RouteClass` carries **per-variant** `#[non_exhaustive]` (`route_class.rs:41-59`),
  which — verified against rustc in that file's own doc comment — makes each
  variant name *unreachable* (E0603) outside `nmp-ownership`: an external crate
  can neither construct (`RouteClass::Automatic`) nor name it in a pattern, only
  a bare `_` arm compiles. `RoutePolicy` (`route_policy.rs:33`) has a **required
  public** `route_class: RouteClass` field, so even though every field is `pub`,
  an external crate **cannot write the struct literal** (it cannot produce a
  value for `route_class`). **`RoutePolicy` is therefore unconstructable outside
  `nmp-ownership` today — by construction, not by convention.** The crate's own
  `route_class.rs:19-27` doc already anticipates the fix: "`nmp-ownership` will
  need to grow a blessed, crate-owned classification API … rather than exposing
  raw variant construction." This note specifies that API.
- **`nmp-router` does not yet depend on `nmp-ownership`** (`nmp-router/Cargo.toml`
  deps: `nostr`, `nmp-grammar`, `nmp-store`). Adding the `nmp-router →
  nmp-ownership` edge is part of E1 and is the direction `relay_source.rs:9-13`
  explicitly blesses.

---

## 1. The blessed classification API — the crux

### 1.1 The problem restated precisely

`nmp-router` (which must mint the default `Automatic` regime) and every future
`nmp-mod-*` crate (nip17 → `VerifiedPrivateInbox`, NIP-29's own exact schemas →
`HostPinned`, drafts → `Automatic`-over-recoverable) need `RoutePolicy` values. They **cannot build one
by literal** — the `route_class` field demands a `RouteClass`, and no code
outside `nmp-ownership` can produce or name one. There is no partial escape: the
sealing is on the *value*, so the only door is a function `nmp-ownership` itself
exposes that **returns an already-assembled value**.

### 1.2 The API: named per-regime constructors on `nmp-ownership`

`nmp-ownership` grows one blessed constructor per *legitimate routing regime*.
Each hard-wires all five fields — source, lanes, fail-mode, **and the class the
caller may not name** — so a caller supplies only the free parameter (the
relay-list kind, or the pinned lane):

```rust
// crates/nmp-ownership/src/route_policy.rs  (NEW: impl block)
impl RoutePolicy {
    /// NIP-17 shape: source = recipient's kind:`list_kind` (10050) inbox list;
    /// app lanes OFF; fail CLOSED; class VerifiedPrivateInbox. read == write.
    pub fn verified_private_inbox(list_kind: u16) -> RoutePolicy { … }

    /// Host-pinned shape for schemas the module itself owns: source =
    /// module-pinned host lane; app lanes OFF; fail CLOSED; class HostPinned.
    pub fn host_pinned(lane: PinnedLane) -> RoutePolicy { … }

    /// Drafts shape (owner-recoverable): source = user's kind:`list_kind`
    /// (10013) draft-relay list; app lanes OFF as additive routes but
    /// re-admitted ON EMPTY (OpenToAppLanes); class Automatic. read == write.
    pub fn app_recoverable(list_kind: u16) -> RoutePolicy { … }

    /// Tooling-only, absent from the app SDK build (§9-decision-4).
    #[cfg(feature = "tooling")] pub fn manual(...) -> RoutePolicy { … }
    #[cfg(feature = "tooling")] pub fn imported(...) -> RoutePolicy { … }
}

// crates/nmp-ownership/src/route_class.rs  (NEW: the ONE bare-class constructor)
impl RouteClass {
    /// The single class CORE ITSELF may mint — the label the default policy
    /// (§2, Unit C) stamps on every unowned-kind route. Every OTHER class is
    /// reachable only baked inside a RoutePolicy constructor above.
    pub fn automatic() -> RouteClass { RouteClass::Automatic }
}
```

**Visibility / where they live:** `pub` associated fns *in `nmp-ownership`*
(the only crate that can name the variants). `nmp-router` reads them for the
default label; `nmp-mod-*` crates call them inside their `claims()` tables.
`Manual`/`Imported` constructors are `#[cfg(feature = "tooling")]` — a new
`tooling` feature on `nmp-ownership`, enabled by `nmp-demo`/diagnostics, **not**
by `nmp-ffi` (the app SDK). The *variants* always exist in the sealed enum; only
their *constructors* are feature-gated, so the SDK build simply has no way to
mint them (`manual_imported_absent_from_sdk_build`, Unit F).

**No arbitrary `RoutePolicy::new(...)`/`custom(...)` is exposed — deliberately.**
A field-by-field constructor would let a caller pick `route_class:
VerifiedPrivateInbox` with `app_lanes: Apply` and `on_empty: OpenToAppLanes` —
the exact K3 leak cell (a gift-wrap-class route that still fans to the app relay
and falls open on empty). Bundling class × lanes × fail-mode per named regime
makes that cell **unconstructable, not merely tested-against.** This is not a
restriction Unit E adds; it is the only API shape the landed #35 sealing permits,
turned into a composition-safety feature. If a genuinely new (source, lanes,
fail-mode, class) tuple is ever needed, it is a **new named constructor added
through review in `nmp-ownership`** — the same "extend the closed vocabulary
through review, never admit a closure" rule that already governs `RelaySource`
(`relay_source.rs:36-37`).

### 1.3 The router never constructs a `RouteClass` — it copies one out

`RouteClass` derives `Copy` (`route_class.rs:38`). *Reading* `policy.route_class`
yields a value the router can move into `WriteStatus::Routed(class, relays)`
(Unit F) or a read-side provenance — reading a `#[non_exhaustive]` field is
allowed; only *naming/constructing* the variant is not. So the router mints
**zero** classes: for an owned kind it copies the class out of the module's
minted `RoutePolicy`; for an unowned kind the default path labels with
`RouteClass::automatic()`. This is why the five non-`Automatic` variants need no
public constructor at all — they are only ever reachable already-baked inside a
`RoutePolicy` a module minted. Clean invariant: **core mints exactly one class
(`Automatic`); every other class is minted only bundled inside a module's
`RoutePolicy` constructor.**

### 1.4 The honesty question — what actually stops an app calling `verified_private_inbox`?

An app links `nmp-ownership` transitively, so it **can** call
`RoutePolicy::verified_private_inbox(10050)`. Rust has no friend-crate
visibility; a `pub fn` is callable by any downstream crate. **The seal is not
"only modules may mint policies" — that would be a fiction. State the real
answer:**

1. **Minting is inert; registration is the act, and registration is the module
   seam, not the app publish surface.** A `RoutePolicy`/`RouteClass` value *does
   nothing* until attached to a `KindClaim` (`route_policy: Some(..)`,
   `kind_claim.rs:25`) and handed to the engine builder as a module registration
   at construction. The two-operation app surface (publish = write intent;
   read = live query; ledger #3 — no untyped route override) has **no parameter that accepts
   a policy**. For a self-minted policy to route anything, the app must register
   a claim at engine-construction — i.e. **act as a protocol module**. "An app
   calls `verified_private_inbox`" collapses to "the app authored an inline
   module," a deliberate, visible, reviewable act at the composition root, never
   an accident on the hot path.
2. **What the sealed `RouteClass` actually guarantees is narrower and real: the
   closed vocabulary.** No code outside `nmp-ownership` can invent a *new* class
   or an *unclassifiable* route. Every route value that exists is one of six
   known regimes, each mapped to an execution+label path the router implements —
   the fail-closed-by-construction property (§3.3: "an unclassifiable publish
   cannot be represented, so it cannot reach the wire"). That holds regardless
   of *who* calls the mint fn. The seal protects the vocabulary's *closedness*,
   not *who may speak it*.
3. **The ownership audit (Unit G) governs whether a registered claim is
   legitimate — by collision, not by policy value.** If an app inline-registers
   a claim on kind:1059 while also linking `nmp-mod-nip17`, the
   engine-construction overlap check errors (two exclusive claims on 1059,
   `kind_scope.rs::overlaps`). If it registers on a kind nobody else owns, it
   **owns** that kind and routes it as declared — which is exactly the module
   doctrine ("linking the composing module is what makes it safe", §9-2). An app
   routing *its own* kind through a fail-closed private policy is not a leak; it
   is the app taking ownership and getting correct private routing. The genuinely
   dangerous cell — a *sensitive* pre-signed kind:1059 routed `Automatic`/public
   because *no* module owns it — is the accepted §9-decision-2 behavior (core
   stays NIP-blind; documented; the fix is "link nip17"). The mint API changes
   none of that calculus.
4. **The Rust-level truth: "module" vs "app" is not expressible in the
   visibility system.** `nmp-mod-*` crates are ordinary crates depending on
   `nmp-ownership`; no type distinguishes them from an app crate depending on
   `nmp-ownership`. There is therefore **no language mechanism** that admits
   `nmp-mod-nip17` to a mint fn while refusing an app crate. The module/app
   boundary is a **workspace / cargo-metadata / audit boundary** (Unit G's
   enrollment scan operates over `nmp-mod-*` crates, `routing-build-plan.md`
   §4.2), enforced at the composition root — never a `pub`-visibility trick.

**Rejected alternative — a sealed capability token** (mint fns take a
`&MintToken` only `EngineBuilder` hands out). Reject it: (a) it fights the
doctrine that an app *may* legitimately author an inline module — the engine
already "blesses" policies by registering claims and running the audit, which is
the real gate; (b) it bolts capability plumbing onto a plain `Copy` data type
meant to be inert values-in-code-after data; (c) it seals the wrong door — an
app that can build a registration can already get its policy honored for kinds
it owns, token or not. The token would seal *construction* while leaving
*registration* open.

**The one-line honest answer:** *apps never route, they publish. Routing
authority is minted only inside a registered, audited `KindClaim`; the sealed
constructors guarantee a closed, leak-safe vocabulary, not a caller whitelist;
and the module/app line is a workspace-audit boundary, not a Rust-visibility
one.*

### 1.5 Promoted boundary: context contribution without schema ownership

Unit E's claim table answers: "what is the schema-wide routing policy for kinds
this module owns?" It does not answer: "what protocol context is this one draft
being published into?"

NIP-29 makes the distinction concrete. Its crate claims only the exact NIP-29
management/state event schemas. When a group publishes a foreign-owned unsigned
draft, NIP-29 may return a new draft with the required `h` tag plus typed
group-host context for that one intent. It does **not** claim the foreign kind or
install `HostPinned` as that kind's global policy. The core validates the
composition, resolves the default signer or an explicit override, signs once,
and uses the ordinary outbox/receipt path. A pre-signed event cannot be mutated
to acquire missing context.

This contextual-publication seam is outside the historical E1/E2 decomposition.
Builders must specify and falsify it separately rather than stretching
`KindClaim` or `RoutePolicy` to cover it.

---

## 2. Claim-table assembly + application (without the router depending on modules)

### 2.1 Assembly — inversion of control at the composition root

The router must never depend on a `nmp-mod-*` crate. So modules do not reach the
router; the **composition root reaches both.** Flow:

1. A module crate exports `pub fn claims() -> &'static [KindClaim]` — plain
   `const`/`static` data (`kind_claim.rs:9-10`), each optionally carrying a
   `RoutePolicy` minted via §1.2.
2. The **app / `nmp-ffi` facade** links exactly the `nmp-mod-*` crates it enables
   and passes their `claims()` into engine construction as
   `Vec<ModuleRegistration>` (a thin newtype over `&'static [KindClaim]`). An app
   enabling zero modules passes an empty vec ⇒ empty table ⇒ today's all-
   `Automatic` behavior (backward-compatible default).
3. **`EngineCore::new` (`core/mod.rs:311`) folds the registrations** into two
   derived structures (this is E2, and it shares the `new` signature change with
   Unit G — see §4):
   - `kind_to_policy: BTreeMap<u16, RoutePolicy>` — each claim's `KindScope`
     (`kind_scope.rs`) expanded over the kinds it owns, mapped to its
     `route_policy` (claims with `route_policy: None` own a kind but do not
     override routing — they still gate the publish path, they just resolve to
     the default policy value carrying `RouteClass::automatic()`).
   - `discovery_list_kinds: BTreeSet<u16>` — `{10002}` ∪ every `kind` any linked
     policy names via `RelaySource::RelayListKind { kind }` (feeds §3).
   - The overlap check (`kind_scope.rs::overlaps` pairwise) returns a **typed
     error** on any exclusive-scope collision among linked modules (Unit G).
4. `kind_to_policy` is handed to `Router::new` (`router.rs:57`, whose signature
   grows a `claims: ClaimTable` param — a small `nmp-ownership`/`nmp-router`
   type wrapping the `BTreeMap`). **The router depends only on `nmp-ownership`
   for the `RoutePolicy` type; it never links a module.** This is the whole
   NIP-blindness mechanism: the per-kind knowledge is *data in a map*, injected,
   never a `match` in core (`routing-and-ownership.md` §4.2's dropped legacy
   per-kind match).

### 2.2 Application per demand atom (READ side — in `Router::compile`)

Classification happens **once, up front**, at the top of `compile`
(`router.rs:76`, the existing Step-1 skeleton grouping is the natural home):

- For each demand atom, inspect its `kinds` set (already a `BTreeSet` field on
  `ConcreteFilter`, and `Skeleton::kinds()` exists, `route.rs:69`):
  - all kinds owned by one policy P ⇒ route the atom under P;
  - kinds unowned ⇒ default path (the landed A/B lanes, unchanged);
  - **mixed** ⇒ **split** the atom into per-policy sub-atoms before routing
    (reuse the skeleton machinery; `kinds` is a set, so splitting is a set
    partition; widen-only `coalesce` may re-merge per relay afterward where the
    resulting filters agree — `coalesce.rs`).
- Under policy P, resolve `P.read_source` (`relay_source.rs:38`):
  - `Nip65Default` ⇒ the existing own-relay candidate assembly
    (`route::build_candidates`, `route.rs:107`) — P is a lanes/class override
    only, same fact source as default.
  - `PinnedLane(lane)` ⇒ `dir.pinned_relays(atom)` (`facts.rs:74`) filtered to
    that lane (converted via a new `From<PinnedLane> for Lane`, the conversion
    `relay_source.rs:24-26` already earmarks). The module pre-ingested the group-
    host relays under `Lane::GroupHost`; the router never knows what a group is.
  - `RelayListKind { kind }` ⇒ **the one real code-reality gap: no accessor
    exists.** The directory today exposes `write_relays`/`read_relays` (both
    10002-bound) and `pinned_relays`, but nothing keyed by an arbitrary
    `(kind, pubkey)`. E1 adds an additive, defaulted-empty
    `RelayDirectory::relay_list(&self, kind: u16, pubkey: &PubkeyHex) ->
    Vec<LanedRelay>` (mirroring the `read_relays` defaulting pattern,
    `facts.rs:110`), read here. Its supplier is §3's generalized ingest.
- Lane applicability: `P.app_lanes` (`route_policy.rs:14`):
  - `Skip` ⇒ do **not** call `app_lane_routes`/`fallback_lane_routes`
    (`router.rs:124,133`) for these atoms. A NIP-17 gift-wrap never touches the
    app relay even though the app lane "takes everything" — *everything* means
    default-routed everything.
  - `Apply` ⇒ apply them as today.
- On empty source: `P.on_empty` (`route_policy.rs:21`):
  - `Closed` ⇒ empty resolved read source ⇒ the atom routes **nowhere** (no wire
    REQ; coverage stays honestly unknown — never a silent app-relay fallback).
  - `OpenToAppLanes` ⇒ empty source re-admits the app/fallback lanes **for this
    atom only**, independent of the `Skip`/`Apply` question (both fields are
    load-bearing and compose: `Skip` = "not additive routes"; `OpenToAppLanes` =
    "the last-resort route when the source is empty").
- **Provenance / class attach:** the resulting `RouteProvenance` (`route.rs:19`)
  entries carry the policy's class. See §5 gap (4): the read side has no
  `RouteClass` home today; E threads `policy.route_class` onto read routes
  (recommended: an `Option<RouteClass>` on `RouteProvenance`, `None` for default-
  path routes that the diagnostics render as `Automatic`).

### 2.3 Application per demand atom (WRITE side — in `resolve_routes`, E2)

`resolve_routes` (`core/mod.rs:658`) grows the claim-table lookup (this rides on
Unit C's `WriteRouting::Default` arm):

- Event kind → `kind_to_policy` lookup (kind is on the signed event, so this
  works for pre-signed publishes too — no template inspection).
  - owned kind ⇒ resolve `P.write_source` (same three-way as §2.2), apply/skip
    app lanes, `Closed`/`Open` on empty (`Closed` mints an empty
    `NarrowOnly`/`PrivateNarrow` ⇒ the existing ledger-#6 fail-closed path at
    `resolve_routes` fires *before* any `PublishEvent`), and stamp
    `WriteStatus::Routed(P.route_class, relays)` (Unit F).
  - unowned kind ⇒ default policy (Unit C's derived union) ⇒
    `WriteStatus::Routed(RouteClass::automatic(), relays)`.
- **Pre-signed publish** (`WritePayload::Signed`, `outbox/mod.rs:39`) is routed
  by the SAME table lookup — this is the §3.3 rule and the reason a pre-signed
  kind:1059 with no nip17 linked routes `Automatic`/public (accepted §9-2;
  documented-leak falsifier, Unit F).

`route_class` provenance thus attaches by *copy-out*: the router/engine reads
`P.route_class` (a `Copy` value it cannot name but can move) into the receipt and
diagnostics; the default path reads `RouteClass::automatic()`.

---

## 3. `sync_discovery` generalization (Q6 — RESOLVED "generalize", `routing-build-plan.md` §7.1)

### 3.1 The gap between spec optimism and the code

The spec says `RelaySource::RelayListKind` bootstraps "with zero new machinery"
because every 1xxxx kind is already a `DiscoveryKind` (`facts.rs:139`,
`0,3 ∪ 10000..=19999`, so 10050/10013 are discovery-eligible). **That is true
for the ROUTING of the discovery sub, but not for two other things the code
hardcodes to 10002:**

- `NIP65_RELAY_LIST_KIND: u16 = 10_002` (`core/mod.rs:65`) is the *only* kind
  `sync_discovery`'s filter ever asks for (`core/mod.rs:1152`).
- `ingest_relay_list_winner` (`core/mod.rs:1168`) re-reads only the kind:10002
  winner and parses it with the NIP-65 write/read split (`:1183,1186`).

So "zero new machinery" holds for the *indexer routing* of the discovery atom
(no router change) but understates the engine work: the queried kind-SET and the
ingest are both 10002-only.

### 3.2 The design (recommended, matches §7.1 Q6)

- Replace the `NIP65_RELAY_LIST_KIND` const's role with the derived
  `discovery_list_kinds: BTreeSet<u16>` field (§2.1) — 10002 becomes one member,
  not a hardcode. For an app with nip17 linked this is `{10002, 10050}`; with
  drafts, add 10013.
- `sync_discovery` (`core/mod.rs:1108`) keeps its whole widen-only structure
  (the triangular-redelivery lesson at `:1075-1097` is untouched); only the
  filter's `kinds` broadens from `[10002]` to `discovery_list_kinds`
  (`:1152`), and the "needed author" predicate generalizes from
  `!knows_write_relays(author)` (`:1121`) to "author is demanded AND some
  referenced list-kind for them is still unresolved." Keep the widen-safe
  simplification the fn already blesses: an author stays in the discovery set
  (querying all list-kinds) until *all* their referenced lists resolve — the
  extra 10050-leg deliveries for a 10050-less author are the same "a few extra
  already-known deliveries, never a structural over-fetch" tradeoff the doc
  already accepts (`:1090-1094`). One discovery sub, all list-kinds, all not-
  fully-resolved authors — routed to indexers by the *unchanged* router
  discovery-kind eligibility.
- `ingest_relay_list_winner` generalizes to dispatch by the arrived kind:
  - `10002` ⇒ today's path (write via `parse_nip65_write_relays`, read via
    `parse_nip65_read_relays` — the ONE NIP-65 read/write MARK split core is
    allowed to know, NIP-01/65 being explicitly core's).
  - `K ≠ 10002` ⇒ a **generic flat `r`-tag URL extraction** into the new
    `(K, pubkey)` directory slot behind `relay_list(kind, pubkey)` (§2.2). This
    keeps core NIP-blind for K≠10002: no read/write marks, no gift-wrap
    knowledge — kind:10050 genuinely *is* a flat list of inbox relays, so a
    generic extractor is correct, not a shortcut. (Structured non-flat list
    parsing, if a future list-kind needs it, is a later module-owned concern —
    see §5 gap/question (2).)

This makes `RelaySource::RelayListKind{10050}` (and `{10013}`) bootstrap through
the existing discovery+indexer machinery with a **bounded, real** addition: one
derived kind-set, one directory slot + accessor, one generic ingest arm. No new
subscription system, no per-policy discovery hook (rejected in §7.1 Q6).

---

## 4. Collision-safe decomposition — E1 (crate-local) vs E2 (core seam)

**Split rationale:** the largest slice of E that touches only `nmp-router` +
`nmp-ownership` (E1) is buildable in a worktree *in parallel* with all the
`core/mod.rs`-heavy work (Unit C, Unit G's constructor change, Unit H, and the
retraction family — `routing-build-plan.md` §6's HIGH-collision file). E2 is the
irreducible `core/mod.rs` remainder and must be sequenced against that hot file.

### E1 — `nmp-ownership` + `nmp-router` only (NO `core/mod.rs`)

**Files:**
- `crates/nmp-ownership/`: the §1.2 constructors (`route_policy.rs` impl,
  `route_class.rs::automatic`), the `tooling` feature in `Cargo.toml`.
- `crates/nmp-router/Cargo.toml`: add the `nmp-ownership` dep (the blessed
  edge, `relay_source.rs:9-13`).
- `crates/nmp-router/src/`: a `From<PinnedLane> for Lane` in `facts.rs`; a
  `ClaimTable` wrapper type + the read-side policy application in `router.rs`
  (classification/splitting, `RelaySource` resolution, `app_lanes` gating,
  `on_empty` composition, class-onto-provenance); the additive
  `RelayDirectory::relay_list(kind, pubkey)` accessor (defaulted-empty,
  `facts.rs`) + `FixtureDirectory::with_relay_list` builder + an optional
  `RouteClass` on `RouteProvenance` (`route.rs`); `lib.rs` re-exports.

**Provable with a test-only fixture claim table** (a made-up owned kind set) +
`FixtureDirectory` — no engine, no `core/mod.rs`.

**Test obligations (E1, headless, `nmp-router`):**
- `route_policy_constructors_are_the_only_external_mint` — a `compile_fail`
  doctest that `RoutePolicy { .. }` and `RouteClass::VerifiedPrivateInbox` do not
  compile from `nmp-router`, but `RoutePolicy::verified_private_inbox(10050)`
  does. (Pins §1.)
- `owned_kinds_route_via_relaylistkind_not_default`.
- `pinnedlane_source_reads_pinned_relays_filtered_to_lane`.
- `apprelay_skipped_for_skip_policy_even_though_it_takes_everything`.
- `filter_mixing_owned_and_default_kinds_is_split_per_policy`.
- `failclosed_empty_read_source_routes_nowhere` vs
  `failopen_empty_read_source_readmits_app_lanes`.
- `skip_and_opentoapplanes_compose` (the K3 leak-cell guard; expanded in Unit I).
- `route_class_attaches_to_read_provenance`.

### E2 — the `core/mod.rs` seam

**Files:** `crates/nmp-engine/src/core/mod.rs` only (plus the derived-field
additions to the `EngineCore` struct).

**Scope:**
1. Fold `Vec<ModuleRegistration>` → `kind_to_policy` + `discovery_list_kinds` at
   `EngineCore::new` (`:311`), pass `kind_to_policy` into `Router::new` (`:314`).
   **This changes the `new` signature — the same signature Unit G changes for
   claim registration and the retraction family changes for struct fields.**
2. Generalize `sync_discovery` + `ingest_relay_list_winner` per §3 (retire the
   10002 hardcode at `:65`,`:1152`,`:1170`).
3. Wire the write-side claim-table dispatch into `resolve_routes` (`:658`) —
   depends on Unit C's `WriteRouting::Default` and Unit F's
   `WriteStatus::Routed(RouteClass, _)`.

**Test obligations (E2, headless, `nmp-engine`):**
- `relaylistkind_source_bootstraps_via_generalized_sync_discovery`.
- `discovery_sub_covers_all_referenced_list_kinds` (10002 ∪ 10050 when nip17
  fixture linked).
- `ingest_kind10050_lands_in_relay_list_slot_not_nip65_write_read`.
- `owned_kind_write_dispatches_through_policy_carrying_its_class`.
- `empty_write_source_under_failclosed_emits_no_publish_effect` (ledger-#6 path).

### Sequencing & what unblocks F and G

```
                 (C: WriteRouting::Default — core/mod.rs)
                        │
A,B (LANDED) ──► E1 ────┼────► E2 ──► (F write-path class threading, G construction gate + audit) ──► I
   (router)   (router+  │      (core seam,   share EngineCore::new + on_signed/resolve_routes edits)
              ownership) │      shares core with C/G/H/retraction)
```

- **E1 is the parallel-safe long pole**: it gives F and G the *types + table +
  read-side application* they build on, and it merges without ever entering
  `core/mod.rs`. Fan it out immediately against the landed A/B substrate.
- **E2 is the serialize-with-core-touches remainder.** Because E2, Unit G, and
  Unit F all edit `EngineCore::new` and the `on_signed`/`resolve_routes` write
  path, and the retraction family edits the same struct/arm
  (`routing-build-plan.md` §6, HIGH collision), **E2 should co-develop with F
  and G in one shared worktree for the `core/mod.rs` seam** (the repo's
  "cohesive change = one shared worktree" rule), or land first and let G/F/
  retraction rebase — whoever lands the `new` signature first wins, the rest
  rebase.
- **Unblocks:** E1 unblocks G's read-gate + the router half of F; E2 unblocks
  G's construction-time overlap error (same constructor) and F's write-path
  class thread. I is the serial tail (needs B, C, E, F).

---

## 5. Spec-vs-code gaps + the genuinely open owner question

**Gaps flagged (real, bounded new code — the spec understates them):**

1. **`RelaySource::RelayListKind` needs a directory accessor that does not
   exist.** "Zero new machinery" is true for the discovery sub's *routing* but
   not for storage/retrieval: there is no per-`(kind, pubkey)` relay-list
   accessor on `RelayDirectory` (only 10002-bound `write_relays`/`read_relays` +
   `pinned_relays`). E1 adds `relay_list(kind, pubkey)` (additive, defaulted);
   §3's generalized ingest supplies it. Bounded, but real.

2. **`sync_discovery` + `ingest_relay_list_winner` are 10002-hardcoded**
   (`core/mod.rs:65,1152,1170,1183,1186`). §3 generalizes them; `routing-build-
   plan.md` §7.1 Q6 flags the discovery half, this note adds the ingest half.

3. **`EngineCore::new`'s signature is edited by E2, Unit G, Unit F, and the
   retraction family** — the single highest merge-conflict point
   (`routing-build-plan.md` §6). Not a spec defect; a coordination fact.
   Recommendation: shared worktree for the core seam (§4).

4. **The read side has no `RouteClass` home.** `RouteProvenance` (`route.rs:19`)
   carries `lane` + `route_kind` but no class; `WriteStatus::Routed` growing the
   class (§3.3) is write-centric — the spec does not explicitly mandate a class
   on read routes. Task item 2 asks how `route_class` attaches to *read* routes;
   E threads it as an `Option<RouteClass>` on `RouteProvenance`. Recommended, but
   note the spec only *requires* it on writes — a minor open point, resolvable at
   build time without an owner.

**Genuinely open owner question (NOT settled by §9 — do not assume a resolution):**

- **Who parses a non-10002 relay-list into relays — generic core, or the owning
  module?** `routing-and-ownership.md` §5 assigns "kind:10050 ingestion" to the
  future `nmp-mod-nip17` crate, but the landed design makes modules **data-only**
  (`claims() -> &'static [KindClaim]`), and no module exists yet, so for the seam
  to be *provable now* something generic must ingest 10050. This note recommends
  **generic flat `r`-tag extraction in core, keyed off the claim table's
  `discovery_list_kinds`** (§3.2) — reading "the module owns kind:10050
  *ingestion*" as "the module owns kind:10050 *semantics* (gift-wrap content),
  while core owns the generic relay-list *fact* extraction," which keeps modules
  data-only and core NIP-blind for the routing fact. That is the doctrine-clean
  reading, but it *is* a genuine fork from a literal reading of §5, and it
  determines whether `ModuleRegistration` ever needs to carry code (an ingest
  hook) or stays pure data. **Surface to owner; recommend generic-core; do not
  presume.**

Everything else the note relies on is already resolved: Q6 (generalize
`sync_discovery`) and Q7 (`nmp-ownership` types crate) are RESOLVED in §7.1 and Q7
is LANDED (#35); `Manual`/`Imported` feature-gating is §9-4; pre-signed-1059-
routes-`Automatic` is §9-2 (accepted). None is re-opened here.

---

## 6. Summary for builders

- **Blessed API:** `RoutePolicy` is unconstructable outside `nmp-ownership`
  (landed #35's per-variant `#[non_exhaustive]` `RouteClass` + required
  `route_class` field). Fix = **named per-regime constructors in `nmp-ownership`**
  (`verified_private_inbox`, `host_pinned`, `app_recoverable`, feature-gated
  `manual`/`imported`) + a single `RouteClass::automatic()` for core's default
  label. The constructors bundle class × lanes × fail-mode so leak cells are
  unconstructable. **Honesty:** the seal guarantees a *closed, leak-safe
  vocabulary*, not a caller whitelist — an app *can* call them, but a policy is
  inert until registered as a claim, the publish surface has no policy parameter,
  and the module/app line is a workspace-audit boundary (Unit G), not a Rust-
  visibility one. A capability token is the wrong tool and is rejected.
- **Claim table:** assembled at the composition root (facade links modules,
  passes `claims()` into `EngineCore::new`), folded to
  `BTreeMap<u16, RoutePolicy>` + `discovery_list_kinds`, injected into
  `Router::new`. **Router depends on `nmp-ownership` only, never a module.**
  Applied per atom: classify-once → split mixed filters → resolve `read/write
  source` (`Nip65Default` reuses `build_candidates`; `PinnedLane` reuses
  `pinned_relays`; `RelayListKind` needs the NEW `relay_list(kind,pubkey)`
  accessor) → `app_lanes` gate → `on_empty` compose → class by copy-out.
- **`sync_discovery`:** the 10002 const becomes a derived kind-SET; filter
  broadens; ingest dispatches by kind (10002 = NIP-65 marks, else generic flat
  `r`-tags into the new slot). Bounded new code, not "zero."
- **E1/E2 split:** **E1** = `nmp-ownership` + `nmp-router` (constructors, dep
  edge, `ClaimTable`, read-side application, `relay_list` accessor) — NO
  `core/mod.rs`, fan out now in parallel. **E2** = `core/mod.rs` (constructor
  fold, `sync_discovery` generalization, write-side dispatch) — shares
  `EngineCore::new`/`resolve_routes` with C/F/G/retraction, so co-worktree or
  serialize. E1 unblocks G's read gate + F's router half; E2 unblocks G's
  construction gate + F's write thread.
- **One owner question:** generic-core vs module-owned parsing of non-10002
  relay-lists (§5). Recommend generic-core; do not presume.
