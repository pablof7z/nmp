# Canonical Rust facade + provisional-surface governance (#52)

Design/plan note for GitHub issue #52 (first implementation frame of epic #43).
This began as the pre-build artifact reviewed by the owner. Its unit/status
annotations now record the implementation that followed. Authoritative
contract lives in `gh issue view 52` / `#43` and `docs/known-gaps.md`
§"Promoted v2 contract gaps"; this note preserves the design and its proof map.

---

## 0. The problem, grounded in the current two surfaces

Today there is no single supported product surface. Two callers assemble the
engine **independently**, and each re-derives its own safety guarantees:

- **`nmp-ffi` (`crates/nmp-ffi/src/facade.rs`)** builds `LiveDirectory`
  (`build_directory`, facade.rs:58), chooses `RedbStore` vs `MemoryStore` from a
  config record (`NmpEngine::new`, facade.rs:97-111), hardcodes its own
  `ROUTER_CAP = 10` (facade.rs:38), calls
  `EngineThread::spawn(store, directory, ROUTER_CAP, PoolConfig::default())`, and
  then drives the raw `nmp_engine::runtime::Handle`.
- **`nmp-demo` (`crates/nmp-demo/src/main.rs`)** does the *same assembly by
  hand*: builds `LiveDirectory` (main.rs:149), always picks `MemoryStore`
  (main.rs:180), hardcodes its own `ROUTER_CAP = 10` (main.rs:51), calls the
  same `EngineThread::spawn` (main.rs:180-185), and drives the raw `Handle`.

Both depend directly on the mechanism crates — `nmp-store`, `nmp-router`,
`nmp-transport`, `nmp-signer`, `nmp-resolver`, `nmp-grammar` (see each
`Cargo.toml`'s `[dependencies]`). So "how to build an NMP app in Rust" is
currently "assemble five mechanism crates correctly," and the FFI is a *second*,
better-guarded product on top.

**The guarantee that provably depends on entry point.** `nmp-ffi`'s
`convert::signed_event_from_ffi` (convert.rs:447-476) runs a caller-supplied
already-signed event through `nostr::Event::verify` before it can become a
`WritePayload::Signed`, and rejects a non-verifying event with a typed
`FfiError::InvalidSignedEvent` — "the engine never sees, and never publishes,
this event" (convert.rs:78-82, 446). But the engine outbox does **not** verify
`Signed` payloads: `crates/nmp-engine/src/outbox/mod.rs` only *declares*
`WritePayload::Signed(SignedEvent)` (mod.rs:39); there is no `verify` anywhere in
`outbox/`. A direct-Rust app calling
`handle.publish(WriteIntent { payload: WritePayload::Signed(evt), .. })` with a
mismatched-id/forged-sig event **publishes it verbatim, unverified**. This is
exactly #52's complaint — a bug-ledger guarantee (#5, "makes ledger #5 honest",
known-gaps §"Four bounded correctness fixes") that only holds if you entered
through FFI. It is the load-bearing motivation and the sharpest parity test.

The post-construction `Handle` (`runtime/mod.rs:789-919`) is *already* a clean
five-verbs-plus-`shutdown`(+`observe_diagnostics`) facade. The gap #52 targets is
**everything around it**: construction, config→mechanism selection, router cap,
signer-from-key, and the semantic guards currently marooned in `nmp-ffi`.

---

## 1. The single facade: shape + home

### Recommendation: a dedicated `nmp` facade crate (`crates/nmp`)

The one supported Rust product surface is a new library crate **`nmp`** that both
`nmp-ffi` and every direct-Rust app depend on. Recommended over "expand
`nmp-engine`'s public API" for three grounded reasons:

1. **A hard dependency boundary, not a documentation promise.** #52 requires
   "resolver/router/store/transport mechanism construction is internal or
   explicitly unstable, not an alternative app contract." If the facade lives in
   `nmp-engine`, apps still transitively see — and today directly depend on — the
   mechanism crates; "internal" is then only a doc claim. A separate `nmp` crate
   lets apps depend on `nmp` alone, with the mechanism crates present only
   transitively. That is enforceable (a surface snapshot of `nmp` is the whole
   product; §5).
2. **`nmp-engine` stays the pure sync/async seam it documents itself to be**
   (lib.rs:1-19: `core` = pure reducer, `runtime` = async edge). The facade adds
   concrete-store selection, nsec parsing, and the Signed-payload verify — all of
   which are *app-assembly* concerns, not reducer/runtime concerns. Keeping them
   out of `nmp-engine` preserves its "nothing depends on it… everything flows
   into it" invariant (lib.rs:16-18) with one deliberate exception: `nmp`.
3. **Naming already exists.** The Swift module is `NMP`, the Kotlin package
   `com.nmp.sdk`; a Rust crate `nmp` is the obvious peer, and the crate name is
   currently free (workspace members list, `Cargo.toml`).

The genuine tradeoff (honest): one more workspace member, and `nmp` must
re-export the value types apps need so they never reach past it. That re-export
list *is* the product surface — a feature, not a cost.

### Facade contents (the two nouns + config + diagnostics + receipts)

`crates/nmp/src/lib.rs` exposes exactly:

- **`EngineConfig`** — lifted out of `nmp-ffi`'s `NmpEngineConfig`
  (facade.rs:44-56), minus the `uniffi::Record` derive: `store_path:
  Option<String>`, `indexer_relays`, `app_relays`, `fallback_relays`. This is the
  ONLY relay/persistence input an app gives (matches the self-bootstrapping-outbox
  contract, facade.rs module doc / known-gaps §M5).
- **`Engine`** — owns the `EngineThread` + `Handle` pair (same
  `Mutex<Option<EngineThread>>` shutdown discipline as facade.rs:84-92).
  - `Engine::new(EngineConfig) -> Result<Engine, EngineError>` — the ONE
    construction call. Owns config→`LiveDirectory` (today's `build_directory`),
    `RedbStore`/`MemoryStore` selection (today's facade.rs:100-111), and the
    router cap (the `ROUTER_CAP = 10` both callers duplicate becomes one facade
    constant).
- **Noun 1 — live query:** `observe(LiveQuery) -> Subscription` (wraps
  `Handle::subscribe`; `Subscription` carries the `QueryHandle` + `Receiver<RowsMsg>`
  and withdraws on `Drop`, folding in `NmpQueryHandle`'s Drop discipline,
  facade.rs:249-253).
- **Noun 2 — write intent:** `publish(WriteIntent) -> Receiver<WriteStatus>` —
  **and this is where the Signed-payload `verify` moves** (see §1.1). Wraps
  `Handle::publish`.
- **Identity:** `add_account(secret_key: &str) -> Result<PublicKey, EngineError>`
  (nsec/hex → `LocalKeySigner` → `Handle::add_signer`, today split across
  facade.rs:125-132 and demo main.rs:155-186) and `set_active_account(Option<PublicKey>)`.
  A lower-level `add_signer(impl SigningCapability)` is retained for NIP-46/bunker
  callers.
- **Diagnostics:** `observe_diagnostics() -> DiagnosticsSubscription`.
- **`shutdown()`**.
- **`EngineError`** — the *semantic* error subset (see §2): `InvalidSecretKey`,
  `StoreOpenFailed`, `InvalidSignedEvent`, `InvalidSignature`.
- **Re-exports** so an app depends on `nmp` alone: the grammar
  (`Filter`/`Binding`/`Derived`/`Selector`/`SetOp`/`IdentityField`/
  `IndexedTagName`, `LiveQuery`), the write plane (`WriteIntent`/`WritePayload`/
  `Durability`/`WriteRouting`), read outputs (`RowDelta`/`AcquisitionEvidence`/
  `RowsMsg`/`WriteStatus`/`DiagnosticsSnapshot`), and `nostr::{PublicKey, Event,
  ...}` as needed. `TagName` was renamed `IndexedTagName` and its whitelist
  removed (#64) — it is the wire/local INDEXED filter key only (`Filter.tags`,
  one ASCII letter, all 52 valid). `Selector::Tag` carries a separate,
  unconstrained `String` for arbitrary already-acquired event-tag names and
  needs no re-export of its own (`String` is already in scope).

### 1.1 The one semantic guarantee that must move into the facade

`Engine::publish` must run `WritePayload::Signed(event)` through
`event.verify()` and return `EngineError::InvalidSignedEvent` on failure —
**for every caller, before the intent reaches the engine.** This is the single
change that makes #52's headline true ("bug-ledger guarantees no longer depend on
entry point"): FFI stops being the sole gate, and direct-Rust inherits the same
guard. `nmp-ffi`'s `signed_event_from_ffi` then shrinks to *string→`nostr::Event`
parsing only*; the verify is inherited, not re-implemented (§2).

Deeper-correct alternative (owner question, §7): push the verify into
`nmp-engine`'s outbox acceptance so even in-crate engine tests and any future
non-facade embedder get it. That is more correct but touches the contested
`outbox`/`core` seam and overlaps the active `build-ffi-signed-publish` work — so
the plan puts it in the facade now and flags the deeper move for coordination.

### What becomes "internal or explicitly unstable"

- Direct construction `EngineThread::spawn(store, directory, cap, pool_config)`
  and the concrete mechanism types (`LiveDirectory::builder`, `RedbStore`,
  `MemoryStore`, `PoolConfig`, `LocalKeySigner`) are no longer an app contract.
- Injecting a pre-built store/directory (needed by `nmp-bdd`, which spawns the
  real `EngineThread` against scripted in-process relays — world.rs module doc)
  stays available as an **explicitly unstable** escape hatch:
  `Engine::from_parts(store, directory, cap, pool_config)` behind a
  `#[doc(hidden)]` + `unstable-mechanism` cargo feature. This is the literal
  realization of #52's "internal or explicitly unstable, not an alternative app
  contract." The `Handle`'s "exactly five verbs" grep-guard (runtime/mod.rs:772,
  plan §5 test 14) is untouched — `Handle` does not widen; the facade wraps it.

---

## 2. `nmp-ffi` becomes thin over the facade

`nmp-ffi` keeps only what is genuinely FFI-boundary and inherits everything
semantic:

- **`facade.rs`** — `NmpEngine` holds a `nmp::Engine` and forwards. Delete
  `build_directory`, the `RedbStore`/`MemoryStore` selection, `EngineThread`/
  `LiveDirectory`/`PoolConfig` imports, and `ROUTER_CAP`. `NmpEngineConfig` maps
  to `nmp::EngineConfig` (it keeps its `uniffi::Record` derive — the FFI wire
  shape — and converts).
- **`convert.rs`** — keeps the *string→typed marshaling* that only exists because
  UniFFI passes strings: `tag_name_from_ffi`, `parse_pubkey`, `parse_relay_url`,
  the `LiteralField` hex validation (convert.rs:179-192, needed because a
  foreign-supplied `Literal` string is unchecked until the boundary), `tags_from_ffi`,
  and the `FfiFilter`/`FfiWriteIntent` mirrors. It **drops the independent
  `event.verify()` call** (convert.rs:472-474); `signed_event_from_ffi` becomes
  parse-only and the verify is done by `nmp::Engine::publish`. `FfiError` gains a
  `From<nmp::EngineError>` mapping so `InvalidSignedEvent`/`InvalidSecretKey`/
  `StoreOpenFailed` surface identically to today.
- **Deps:** drop `nmp-store`, `nmp-router`, `nmp-transport`, `nmp-resolver` from
  `nmp-ffi/Cargo.toml` (now transitive via `nmp`); keep `nmp-grammar` (type
  mirrors) and add `nmp`.
- `nmp-demo` migrates to `nmp::Engine::new(EngineConfig { .. })` and drops its
  direct mechanism-crate deps; the follow-feed query builder (main.rs:436) uses
  the grammar re-exported from `nmp`.

Result: FFI and direct-Rust are the same product; the only FFI-specific code is
type mirroring and string parsing.

---

## 3. Parity tests (acceptance evidence item 1)

**Implementation status (2026-07-11): Unit D is implemented by this change.**
The executable proof lives in `crates/nmp-parity`; Units E/F subsequently
landed the reproducible baselines and enforcement described below.

**Strategy: same operations, two entry points, identical observables, against
isolated instances of one shared loopback-relay implementation.** Reuse
`nmp-bdd`'s real in-process `ScriptedRelay` (world.rs / relays.rs) — both
surfaces point at real `ws://127.0.0.1:port` URLs, so no mock and no
mechanism-injection is needed for parity itself. The only added relay seams
seed a pre-signed event verbatim and seed the author's NIP-65 relay list; the
relay implementation is not copied.

A parity driver expresses each scenario abstractly (configure the scripted
relay as the indexer; add account; open a bounded empty custom-kind query and
wait for the seeded NIP-65 fact to discover that same relay as the author's
read/write relay; cancel the preflight; observe the bounded literal content
query; publish an intent) and runs it twice. The explicit discovery-settle
phase keeps discovery/recompile traffic from racing the content snapshot; the
limited queries honestly retain `coverage: None`, avoiding any comparison of
uncontrolled wall-clock watermarks.

1. **Direct Rust:** `nmp::Engine::new(EngineConfig { indexer_relays: [scripted], .. })`,
   driving the facade nouns, folding `RowDelta`s into a row set exactly as
   world.rs's `FeedState` does.
2. **FFI:** `nmp_ffi::NmpEngine::new(NmpEngineConfig { .. })` (which internally
   builds the same `nmp::Engine`), driven through the FFI types + `RowObserver`/
   `ReceiptObserver`.

Assert identical: accumulated rows, `AcquisitionEvidence`, ordered
`WriteStatus` receipt sequence, and `DiagnosticsSnapshot` shape. The successful
path uses real loopback REQ/EVENT delivery, live NIP-65 discovery, limited
source-plan evidence, and a durable write receiving the relay's OK and reaching
`Acked`. **The load-bearing case follows the ratified A0
contract, which supersedes the earlier synchronous-error sketch:** publishing a
tampered `WritePayload::Signed` produces `Failed` as the first and only receipt
fact on both surfaces, with no `Accepted` and no EVENT/REQ reaching the relay.

Home: `crates/nmp-parity`, a product-level dev/test crate separate from the BDD
step catalog. It depends on `nmp` + `nmp-ffi` + the exported scripted-relay
helper.

---

## 4. Drift-detection CI (acceptance evidence: "CI detects projection drift")

Three layers, cheapest first:

1. **Rust → Swift/Kotlin (mechanical projection).** UniFFI proc-macro mode
   (`uniffi::setup_scaffolding!()`, lib.rs:20) generates `gen/nmp_ffi.swift` +
   modulemap + header and the Kotlin bindings; these are deliberately *not*
   committed and rebuilt clean by `scripts/build-swift-xcframework.sh` /
   `build-kotlin-jvm.sh`. The existing `swift-package` / `kotlin-package`
   clean-clone jobs (`.github/workflows/ci.yml`) already fail when regenerated
   bindings stop matching the hand-written wrappers (`Packages/NMP/Sources/NMP/*.swift`,
   `Packages/NMPKotlin/src/main/kotlin/...`). **Strengthen:** add a committed
   *component-interface snapshot* of `nmp-ffi`, extracted from the compiled
   library's proc-macro metadata in UniFFI library mode (truthfully not UDL),
   and a fast Rust-job test that regeneration matches it — so an FFI-shape
   change is caught in the quick job, not only the slow macOS one. **Implemented
   by Unit E.**
2. **Facade ↔ FFI (semantic projection).** The §3 parity harness *is* the drift
   detector between the two Rust entry points: if FFI drifts from facade
   behavior, parity fails.
3. **Surface inventory (public-shape projection).** Committed public-surface
   snapshots for `nmp` and `nmp-ffi` (§5), regenerated and diffed in CI.

---

## 5. Change-governance protocol, made real (the mechanism I designed)

The lightest thing that makes #52's "visible signoff trail" *enforced*, not
aspirational — three cooperating pieces:

**(a) Committed surface snapshots.** `docs/surface/nmp-facade.txt` (the
rustdoc-derived public item inventory of `nmp`, including compiler-resolved
definitions and their reachable dependency-owned type closure for explicit
reexports (including public inherent methods only on explicit root reexports,
but excluding nested helper and trait/auto/blanket impl inventories) via the
exact-PackageId, locked #89 tool) and
`docs/surface/nmp-ffi-component.txt` (the language-independent UniFFI
proc-macro component interface, extracted from the compiled library in library
mode—not UDL). Regenerated by `scripts/regenerate-surface-snapshots.sh` and
committed. A change to the product surface is now a *visible diff* in these
files. Both the component extractor and Rust reexport resolver are standalone,
own-lockfile tools outside `nmp-ffi` and the product workspace; steady-state CI
sources them from the PR base. **Implemented by Unit E, corrected by #89.**

**(b) An append-only change log.** `docs/surface-change-log.md`, one entry per
public Rust/FFI/Swift/Kotlin shape change, with the required fields from #43/#52
verbatim:
1. **Failure evidence** — the falsifier/finding that forced the change (a shape
   may not change without one).
2. **Cross-surface impact** — Rust / FFI / Swift / Kotlin.
3. **Persistence impact** — store/journal/redb-key effects.
4. **Diagnostics impact** — snapshot/coverage effects.
5. **Updated falsifiers** — links to the synchronized tests.
6. **Superseded path removed** — what was deleted (enforces "no compat alias /
   parallel semantic path"; an ADD without a corresponding REMOVE is visible in
   the snapshot diff for the reviewer to reject).
7. **Human signoff** — approver + PR/date. **Implemented by Unit F; prior
   entries are byte-for-byte immutable.**

**(c) The enforcing CI gate `surface-governance`.** Regenerate the §5(a)
snapshots; if they differ from committed, the job fails unless **both** the
snapshot files **and** `docs/surface-change-log.md` changed in this PR's diff vs
base (`git diff --name-only origin/master...HEAD`). That single git-diff check is
what turns the protocol from documentation into a gate: no public surface moves
without a log entry, and PR review (the human signoff) approves the snapshot delta
+ the entry together. The implemented gate uses explicit base/head commits,
rejects stale regeneration, requires every appended entry field, and verifies
that the base log is an exact byte prefix of the head log. It also governs the
hand-written Swift/Kotlin public wrapper paths when snapshots do not move and
their consumer package/build/settings manifests; it allows explicit
correction-only appends.

The policy gate runs as a `pull_request_target` workflow loaded from the default
branch, extracts executable governance files from the PR base, treats the head
only as git data, and rejects changes to that protected program. Deterministic
regeneration runs in a separate ordinary `pull_request` job; the trusted gate
protects that workflow and its scripts byte-for-byte. The first Unit E/F PR is
necessarily a bootstrap because its base has no trusted workflow; it seeds only
the real #67/#73/#77 history, merges under existing CI/manual review, and #81
then requires both stable checks in branch protection.

Plus the human-facing half: a `.github/pull_request_template.md` checklist
mirroring the seven fields. The CI gate guarantees the entry exists; the template
guides its content; review supplies the signoff.

Docs alignment (closes the "docs identify one facade" acceptance item): `README.md`
and a short `docs/architecture/` note name `nmp` as *the* Rust product surface and
mark the mechanism crates internal/unstable; the known-gaps §"Public syntax
remains provisional" bullet is updated to point at this now-enforced protocol.

---

## 6. Collision-safe decomposition (units, order, contested seams)

| Unit | Scope / files | Test obligations | Depends on | Touches contested seam? |
|---|---|---|---|---|
| **A. Facade crate** | new `crates/nmp/*`; workspace `Cargo.toml`. `EngineConfig`, `Engine::new`, two nouns, diagnostics, `add_account`/`set_active_account`, **`publish` Signed-verify**, `EngineError`, re-exports, unstable `from_parts`. | config→store/directory selection; `add_account` from nsec+hex; **tampered `Signed` publish → `InvalidSignedEvent`**; shutdown idempotency. | — (first) | No |
| **B. FFI rethread** | `crates/nmp-ffi/{facade,convert,lib,types}.rs`, `Cargo.toml`; regenerate `gen/*`. FFI wraps `nmp::Engine`; drop independent verify + mechanism deps; `From<EngineError>` for `FfiError`. | existing `convert.rs` tests stay green; verify inherited (tampered `Signed` still rejected via facade). | A | **Yes — FFI seam. Coordinate with `build-ffi-signed-publish`.** |
| **C. Demo migration** | `crates/nmp-demo/src/main.rs`, `Cargo.toml`. Replace hand-assembly with `nmp::Engine::new`. | existing `parse_args`/query-builder tests stay green; runs against real relays. | A | No |
| **D. Parity harness** | `crates/nmp-parity`. Same ops via facade + FFI over isolated instances of the shared scripted relay. | identical rows/`AcquisitionEvidence`/ordered receipts/diagnostics; tampered-`Signed` `Failed`-first-and-only with zero relay contact. | A, B | Implemented by this change; reuses `nmp-bdd` helper without copying it. |
| **E. Drift CI + snapshots — implemented** | `docs/surface/*`, `scripts/regenerate-surface-snapshots.sh`, `.github/workflows/ci.yml` (proc-macro component snapshot + Rust inventory). | snapshot regen matches committed; Rust or FFI shape drift fails the fast job. | A, B | No |
| **F. Governance — implemented** | `docs/surface-change-log.md`, `.github/pull_request_template.md`, `surface-governance` gate, README/architecture/known-gaps edits. | stale snapshot, missing append, historical edit/delete fail; valid paired update passes. | E | No |

**Order:** A → (B ∥ C) → D; E after A+B; F after E. **A is the chokepoint.** The
only contested-seam unit is **B** (the FFI seam the team-lead flagged), which
overlaps active `build-ffi-signed-publish`; sequence B after that agent's
signed-publish work lands, or co-own the `convert.rs`/`facade.rs` edit. No unit
needs to edit `nmp-engine/src/core/mod.rs` (the facade owns the verify), which
keeps this frame off the most-contested file unless the owner chooses the
deeper-outbox alternative (§7 Q2).

---

## 7. Honesty: ambiguities needing owner input, and invariant collisions

- **Q1 — Facade home (structural, load-bearing).** Recommendation: dedicated
  `nmp` crate. The alternative (expand `nmp-engine`'s public API) is smaller but
  can't give a hard dependency boundary — flagged for signoff because it adds a
  workspace member and defines the product surface.
- **Q2 — Where the `Signed`-payload verify ultimately lives.** Facade `publish`
  (this plan) makes it entry-point-agnostic for all *app* surfaces. Pushing it
  into `nmp-engine`'s outbox acceptance is deeper-correct (covers in-crate engine
  tests and any future non-facade embedder) but touches the contested `outbox`/
  `core` seam and overlaps `build-ffi-signed-publish`. Needs an owner/coordination
  call. #43/#52 do not pin this.
- **Q3 — Unstable-mechanism escape hatch.** `nmp-bdd` spawns the real
  `EngineThread` against scripted relays with an injected `MemoryStore`
  (world.rs). Confirm the `#[doc(hidden)] from_parts` + `unstable-mechanism`
  feature is the accepted form of #52's "explicitly unstable" — vs migrating
  `nmp-bdd` to construct via loopback URLs only (heavier test rewrite).
- **Q4 — Surface-snapshot tooling.** Hand-rolled `pub`-item inventory (zero new
  deps, brittle) vs a dev-dependency like `cargo-public-api`/`cargo-semver-checks`
  (robust, heavier). Owner tooling preference.
- **Q5 — Parity harness home.** New `crates/nmp-parity` (recommended, avoids
  `nmp-bdd` collision) vs extending `nmp-bdd`'s step catalog. Minor.
- **Invariant check — no collision found with "exactly five verbs."** The facade
  wraps `Handle`; it does not widen it, so the runtime/mod.rs:772 grep-guard (plan
  §5 test 14) stays valid. The "one facade" goal is consistent with the existing
  Swift/Kotlin hand-written ergonomic wrappers: those remain *projections* of the
  one Rust surface via UniFFI, and the §5 governance review is what keeps them in
  lockstep.

---

## Fable checkpoint (verdict)

**GO — with required changes.** The plan's load-bearing finding is real, its
structure is sound, and the governance mechanism is the lightest enforceable
one. One decision (Q2) overrides the plan's placement, and it changes the unit
map: the verify moves to the engine acceptance boundary, which adds one small
unit that edits `nmp-engine/src/core/mod.rs`.

### Verified against code (not taken on assertion)

The security crux checks out end-to-end:

- `nmp-ffi/src/convert.rs:472-474` is the **only** `Event::verify` on the
  publish path anywhere in the workspace; `grep verify crates/nmp-engine/src/`
  returns zero code hits.
- `EngineCore::on_publish` (core/mod.rs:530) emits `WriteStatus::Accepted`
  **before it even matches on the payload**, then routes
  `WritePayload::Signed(event)` straight into `on_signed` (core/mod.rs:565) →
  `resolve_routes` → `Effect::PublishEvent(relay, event)` per relay. A
  direct-Rust `handle.publish(WriteIntent { payload: Signed(forged), .. })`
  publishes a forged event verbatim today. Confirmed, not asserted.
- `WriteStatus::Failed(String)` (outbox/mod.rs) is already documented as the
  "whole-intent terminal reached BEFORE any relay was ever contacted — a
  signer rejection" variant — the exact precedent a verify-rejection needs.
- `build-ffi-signed-publish` **merged** (#41, commit 7af2f53), so the plan's
  Unit-B/Q2 "overlaps active work" caveat is stale.

### The five decisions

**Q1 — Facade home: new `crates/nmp` crate. Ratified as recommended.**
The whole point of #52 is that "supported surface" stops being a documentation
claim. Only a separate crate makes it a *dependency-graph* fact: an app's
`Cargo.toml` names `nmp` and nothing else, and the committed surface snapshot
of `nmp` **is** the entire product — auditable, diffable, gateable. Expanding
`nmp-engine` instead would leave the mechanism crates one `use` away with no
visible trail, and would pollute the engine's documented "pure reducer + async
edge, nothing else" identity (lib.rs:1-19) with app-assembly concerns
(store selection, nsec parsing, router caps). The extra workspace member is
the cost of enforceability; pay it. Nothing prevents an app from *also*
depending on `nmp-engine` — that is unavoidable with published crates and is
exactly what #52's "explicitly unstable, not an alternative app contract"
wording anticipates; docs + governance mark everything below `nmp` unstable.

**Q2 — Signed-verify placement: the acceptance boundary, NOT the facade.
This overrides the plan.** The ONE verify lives in `EngineCore::on_publish`,
on the `WritePayload::Signed` arm, **before `WriteStatus::Accepted` is
emitted**. Rejection is fail-closed: the intent terminates as
`WriteStatus::Failed` (typed reason), never reaches `on_signed`, never
produces a `PublishEvent` effect. Rationale, in order of force:

1. *Facade placement recreates the exact bug class #52 exists to kill, one
   layer down.* `Handle` is public in `nmp-engine`; `nmp-bdd` spawns
   `EngineThread` directly (world.rs:503); in-crate engine tests and any
   `from_parts` holder drive raw `Handle::publish`. With facade-only verify,
   "the guarantee depends on entry point" is still true — the entry points
   just moved. Acceptance is where every publish path converges; it is the
   only placement that makes #52's headline literally, unconditionally true.
2. *It is the only placement that composes with the #2/#3 crash-safe Accepted
   boundary.* Under #2/#3, "durable Accepted atomically owns" the frozen
   event, and crash recovery replays from the journal. If a forged event can
   reach acceptance, the journal durably owns garbage and recovery republishes
   it with no verify in the replay path. Verify-before-Accepted gives the
   crash-safe design a free invariant: *everything the journal owns is
   publishable verbatim*. This must be recorded as an input invariant to the
   #2/#3 design: for the pre-signed lane, "frozen unsigned event, expected
   pubkey" extends to "or a **verified** signed event."
3. *The deferral reason is gone.* The plan parked the deeper move only
   because of the `build-ffi-signed-publish` overlap; #41 merged.
4. *Purity and cost are non-issues.* `Event::verify` is deterministic,
   IO-free schnorr verification — it fits the pure-reducer discipline, and
   `nmp-transport` already runs the same check per inbound event at far
   higher volume. Trust posture becomes symmetric: events are verified
   wherever they enter the engine's custody, inbound (transport ingest) and
   outbound-presigned (acceptance) alike.

Consequences (these are the required changes, not options):

- **No duplicate verify at the facade or FFI.** Per the no-parallel-path rule,
  `nmp-ffi`'s `convert.rs:472-474` verify and the
  `FfiError::InvalidSignedEvent` variant are **deleted** in Unit B
  (superseded-path-removed), and `signed_event_from_ffi` becomes parse-only as
  planned. String-shape parse failures (`InvalidEventId`, `InvalidSignature`
  as *parse* error, `InvalidTag`) stay synchronous at the FFI boundary —
  those are marshaling, not the invariant.
- **The failure surfaces on the receipt stream**, as the first and only
  status: `WriteStatus::Failed(..)` with no preceding `Accepted` — "Accepted"
  now *means* "the engine took ownership," which is exactly the #2/#3
  semantics. Facade `publish` stays a thin forwarder returning
  `Receiver<WriteStatus>`; drop `InvalidSignedEvent`/`InvalidSignature` from
  the sync `EngineError` set (§1 list shrinks to `InvalidSecretKey`,
  `StoreOpenFailed`, + construction errors).
- **This changes #41's just-merged FFI behavior** (sync typed error → stream
  `Failed`). That is a governed public-surface change: it becomes the **first
  real entry in `docs/surface-change-log.md`** — failure evidence is the
  direct-Rust bypass verified above; superseded path is the convert.rs verify;
  updated falsifiers are #41's tests reshaped to assert the stream terminal.
  Fitting that the governance protocol's inaugural entry is the change that
  motivated the epic.
- **Ephemeral intents:** verify runs for ALL durability classes (rejection
  must precede the wire), but an Ephemeral intent has no sink, so a forged
  ephemeral publish fails *silently* — same as ephemeral route failures
  today. Document this explicitly in the facade `publish` doc; optionally
  count rejections in `DiagnosticsSnapshot` (nice-to-have, not gating).

**Q3 — `from_parts` + `unstable-mechanism` feature: accepted.** #52 says
"internal **or explicitly unstable**" — it demands a marked boundary, not
impossibility. A `#[doc(hidden)]` constructor behind a named cargo feature is
the strongest marking Rust offers short of visibility hacks: enabling it is a
greppable, reviewable line in a consumer's `Cargo.toml`. And with Q2 resolved
at acceptance, the hatch is no longer even a *security* hole — a
`from_parts`-built (or raw-`EngineThread`) engine still verifies at
acceptance; the hatch is only a *stability* exception, which is exactly what
the feature name declares. Do NOT rewrite `nmp-bdd` to loopback-URLs-only:
heavy churn, zero invariant gain, and `nmp-bdd` is in-workspace test infra
that may legitimately keep mechanism deps. `from_parts` exists so no
*out-of-workspace* consumer ever needs them.

**Q4 — Surface-snapshot tooling: `cargo-public-api`, pinned, not
hand-rolled.** The governance gate is only as strong as the snapshot's
fidelity. A hand-rolled `pub`-item grep misses enum-variant additions, field
changes, trait impls, and re-export resolution — precisely the quiet drifts
the gate exists to catch; a gameable snapshot makes §5 theater.
`cargo-public-api` diffs the true rustdoc-derived surface. Contain its cost:
pin the tool version (`cargo install --locked cargo-public-api@<ver>`) and
the nightly toolchain it needs for rustdoc JSON, in the `surface-governance`
job only — the rest of CI stays stable-toolchain. The FFI surface snapshot
stays as planned (generated `gen/nmp_ffi.swift` declaration surface /
uniffi metadata — mechanical, not hand-rolled).

**Q5 — Parity home: new `crates/nmp-parity`. Ratified.** Parity is a
product-level harness (two entry points, one scenario), not a cucumber step
catalog; `nmp-bdd` is shared, actively touched infrastructure — collision
avoidance is decisive. Requirement: reuse `nmp-bdd`'s `ScriptedRelay` via an
exported helper (public-but-`#[doc(hidden)]` module or tiny shared test
crate); do not fork it.

### Contract validation

- **"One invariant-preserving surface" — honored, with Q2 strengthening it.**
  With facade-only verify it would NOT have been (raw `Handle` bypass);
  with acceptance-verify, no reachable path — facade, FFI, `from_parts`, raw
  `EngineThread`, in-crate test — can publish or journal an unverified
  pre-signed event. The `from_parts` hatch is a stability exception only.
- **Governance is genuinely enforced and is the lightest thing that works.**
  The same-PR git-diff check (snapshots + change-log must move together)
  turns the protocol into a gate; the human signoff is PR review of that
  paired diff. It is gameable only by a reviewer approving a garbage log
  entry — i.e., by the signoff itself failing, which no mechanism prevents.
  Cheap strengthening the builder should include: require the change-log diff
  to have **added lines** (`git diff --numstat`), not merely be touched, and
  run the gate with `fetch-depth: 0` so `origin/master...HEAD` resolves.
- **Unit decomposition — sound, with one amendment.** The plan's claim "no
  unit needs `core/mod.rs`" was true only under facade-placement; Q2 voids it.
  Amended unit map:
  - **New Unit A0 (small, first):** `nmp-engine/src/core/mod.rs` — verify on
    the `Signed` arm of `on_publish` before `Accepted`; core falsifier test
    (tampered event → `Failed` terminal, no `Accepted`, no `PublishEvent`
    effect). ~tens of lines. **This is the contested core seam** — sequence
    it before everything else and coordinate with `design-crashsafe-accepted-2-3`
    (their journal design inherits verified-at-acceptance as an invariant)
    and any routing-unit work touching `core/mod.rs`.
  - Unit A: facade as planned, minus the verify; `EngineError` trimmed.
  - Unit B: as planned, plus deleting the convert.rs verify +
    `InvalidSignedEvent` variant and reshaping #41's falsifiers; the
    "coordinate with `build-ffi-signed-publish`" note is stale (merged) —
    B is unblocked.
  - Units C/D/E/F unchanged; D's load-bearing parity case becomes: tampered
    `Signed` publish yields the identical `WriteStatus::Failed`-first receipt
    stream on both surfaces.
  - **Order: A0 → A → (B ∥ C) → D; E after A+B; F after E.**

### Residual risk

1. **Core-seam collision** — A0 edits `core/mod.rs` while #2/#3 is being
   designed against the same acceptance seam. Mitigation: A0 is tiny and
   lands first; the #2/#3 designer is notified that verify-before-Accepted is
   now a fixed input invariant.
2. **FFI behavior change on a just-merged surface** (#41 sync error → stream
   terminal) ripples into Swift/Kotlin falsifiers. Bounded, and it exercises
   the new governance protocol end-to-end on its first real change.
3. **`cargo-public-api` nightly-rustdoc dependency** can break on toolchain
   bumps; pinning contains it, and the failure mode is a red governance job,
   never a silent gate bypass — fail-closed, acceptable.
4. Ephemeral forged publishes fail silently (no sink). Documented behavior,
   consistent with ephemeral route failures; diagnostics counter optional.

— Fable, design checkpoint, 2026-07-11.
