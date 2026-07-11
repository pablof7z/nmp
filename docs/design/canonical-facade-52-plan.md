# Canonical Rust facade + provisional-surface governance (#52)

Design/plan note for GitHub issue #52 (first implementation frame of epic #43).
**No code is written here.** This is the artifact the owner reviews before any
building. Authoritative contract lives in `gh issue view 52` / `#43` and
`docs/known-gaps.md` §"Promoted v2 contract gaps"; this note only designs how to
satisfy it, grounded in the current code.

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
  (`Filter`/`Binding`/`Derived`/`Selector`/`SetOp`/`IdentityField`/`TagName`,
  `LiveQuery`), the write plane (`WriteIntent`/`WritePayload`/`Durability`/
  `WriteRouting`), read outputs (`RowDelta`/`QueryCoverage`/`RowsMsg`/
  `WriteStatus`/`DiagnosticsSnapshot`), and `nostr::{PublicKey, Event, ...}` as
  needed.

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

**Strategy: same operations, two entry points, identical observables, against
shared loopback relays.** Reuse `nmp-bdd`'s real in-process `ScriptedRelay`
(world.rs / relays.rs) — both surfaces can point at their real
`ws://127.0.0.1:port` URLs, so no mock and no mechanism-injection is needed for
parity itself.

A parity driver expresses each scenario abstractly (configure indexer =
scripted-relay URL; add account; observe `$myFollows`; publish an intent) and
runs it twice:

1. **Direct Rust:** `nmp::Engine::new(EngineConfig { indexer_relays: [scripted], .. })`,
   driving the facade nouns, folding `RowDelta`s into a row set exactly as
   world.rs's `FeedState` does.
2. **FFI:** `nmp_ffi::NmpEngine::new(NmpEngineConfig { .. })` (which internally
   builds the same `nmp::Engine`), driven through the FFI types + `RowObserver`/
   `ReceiptObserver`.

Assert identical: accumulated feed rows, `QueryCoverage`, ordered `WriteStatus`
receipt sequence, and `DiagnosticsSnapshot` shape. **Must include the
load-bearing case:** publishing a tampered `WritePayload::Signed` fails
identically on both surfaces (`EngineError::InvalidSignedEvent` ≙
`FfiError::InvalidSignedEvent`) — the falsifier that proves the guarantee now
lives in the shared facade, not in FFI alone.

Home: a new `crates/nmp-parity` dev/test crate (recommended over extending the
shared `nmp-bdd`, which other agents touch — collision avoidance). It depends on
`nmp` + `nmp-ffi` + the scripted-relay helper.

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
   *component-interface snapshot* of `nmp-ffi` (extracted via `uniffi-bindgen`'s
   metadata, or by snapshotting `gen/nmp_ffi.swift`'s declaration surface) and a
   fast Rust-job test that regeneration matches it — so an FFI-shape change is
   caught in the quick job, not only the slow macOS one.
2. **Facade ↔ FFI (semantic projection).** The §3 parity harness *is* the drift
   detector between the two Rust entry points: if FFI drifts from facade
   behavior, parity fails.
3. **Surface inventory (public-shape projection).** Committed public-surface
   snapshots for `nmp` and `nmp-ffi` (§5), regenerated and diffed in CI.

---

## 5. Change-governance protocol, made real (the mechanism I designed)

The lightest thing that makes #52's "visible signoff trail" *enforced*, not
aspirational — three cooperating pieces:

**(a) Committed surface snapshots.** `docs/surface/nmp-facade.txt` (the public
item inventory of `nmp`) and `docs/surface/nmp-ffi.udl.txt` (the FFI component
interface). Regenerated by a `scripts/regenerate-surface-snapshots.sh` and
committed. A change to the product surface is now a *visible diff* in these
files.

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
7. **Human signoff** — approver + PR/date.

**(c) The enforcing CI gate `surface-governance`.** Regenerate the §5(a)
snapshots; if they differ from committed, the job fails unless **both** the
snapshot files **and** `docs/surface-change-log.md` changed in this PR's diff vs
base (`git diff --name-only origin/master...HEAD`). That single git-diff check is
what turns the protocol from documentation into a gate: no public surface moves
without a log entry, and PR review (the human signoff) approves the snapshot delta
+ the entry together.

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
| **D. Parity harness** | new `crates/nmp-parity`. Same ops via facade + FFI over shared scripted relays. | identical feed/coverage/receipts/diagnostics; tampered-`Signed` parity. | A, B | Reuses `nmp-bdd` scripted-relay helper — coordinate. |
| **E. Drift CI + snapshots** | `docs/surface/*`, `scripts/regenerate-surface-snapshots.sh`, `.github/workflows/ci.yml` (UDL snapshot test + inventory). | snapshot regen matches committed; FFI-shape change fails fast job. | A, B | No |
| **F. Governance** | `docs/surface-change-log.md`, `.github/pull_request_template.md`, `surface-governance` gate, README/architecture/known-gaps edits. | same-PR-entry gate fails when snapshot changes without a log entry. | E | No |

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
