# MR — Routing & Ownership: the build plan

- **Date:** 2026-07-11
- **Status:** Build plan (pre-build). Decomposes the canonical spec
  `docs/design/routing-and-ownership.md` (Parts A/B/C, §9 RESOLVED decisions
  authoritative) into dependency-ordered, fan-out-able build units. Writes no
  code; this is the artifact that gets fanned out to builders.
- **Promotion status:** historical execution record. For unbuilt units, the promoted
  contracts supersede kind-ownership and untyped-override assumptions here.
- **Source of truth:** `docs/design/routing-and-ownership.md` §9 (owner-resolved
  decisions) overrides §8 (open list) wherever they conflict. Where §9 is silent
  and §8 is open, the sub-decision is surfaced in §7 below as an owner question —
  this plan invents no resolutions.
- **Substrate read:** `VISION.md` (milestone/kill style), `bug-class-ledger.md`
  (#14 proposed), `known-gaps.md` (DM inbox / authorless / decrypt / AUTH),
  `docs/design/retraction-and-negative-deltas.md` (the co-pending family this
  must sequence against).

---

## 1. Milestone framing (VISION style)

**MR — Routing & Ownership.** The completion of VISION **P4** ("routing
correctness is the engine's mission, and it is not optional") and the extension
of VISION **P7** ("a bug-class ledger replaces governance-by-policing") to the
kind-ownership boundary.

**Thesis (what MR proves).** Every relay-bearing decision the engine makes — a
read route, a write route, a private-inbox route, a group-host route, a
discovery route, a fallback top-up — is **compiler output from lane-typed facts
plus positive schema-ownership claims or typed per-intent protocol context**, with (a) no untyped relay override on the default path
(ledger #3 intact), (b) no per-kind `match` in core (the claim table *is* the
per-kind knowledge), (c) core knowing **zero NIPs** beyond NIP-01/65 defaults
(gift-wrap/group/draft routing all live in module crates), and (d) the
ownership boundary enforced by a **cargo-metadata-driven static audit plus
types** — never a lint, never the legacy linker-symbol gimmick, never a regex
source scan. MR is the proof that the two-noun surface's *routing half* is
general and that VISION P7's "surface change, not lint" discipline holds for
ownership too.

**Pre-committed kill conditions** (either firing means stop and re-open the
spec, not patch around it):

- **K1 — the override vocabulary is not general.** Routing a real second
  module's kinds (NIP-17 is the forcing function) requires either an app/module
  **closure** in the routing decision, or a **NIP-specific branch inside core**
  (core learning what a "gift wrap" or a "group" is), because the closed
  `RelaySource` vocabulary (`Nip65Default | RelayListKind | PinnedLane`) cannot
  express the module's inbox/host routing. That falsifies Part B's "values in,
  code after" bet — the router would be NIP-aware, and the modularity claim in
  §5 of the spec is paste. (Mirrors VISION M1's "grows the kind:3 case and the
  39002 case instead of one mechanism" kill.)
- **K2 — the ownership boundary cannot be enforced structurally.** If exclusive
  kind-ownership + "route authority ⊆ ownership" cannot be made a red build by
  **types + one cargo-metadata CI test**, and instead needs a source-text scan,
  a doctrine-lint, or a hand-maintained per-kind match to be safe, then the
  ledger-not-lint thesis (VISION P7) fails for bug-class #14 and MR has
  re-grown the exact governance apparatus the new repo exists to avoid.
- **K3 (secondary, non-thesis — the composition matrix rots).** The lanes ×
  overrides × fail-modes × pre-signed × p-tag-exclusion decision *table* has a
  leak cell that cannot be closed by field composition — e.g. `AppLanes::Skip`
  and `FailMode::OpenToAppLanes` cannot be made to compose without a gift-wrap
  reaching the app relay, or a pre-signed kind:1059 sliding through `Automatic`
  in an app that forgot nip17. This is execution risk, not bet risk (fixed, not
  abandoned) — but it is the spec's own "biggest risk" (§7 there) and its
  mitigation (the enumerated decision-table tests, Unit I) is non-negotiable.

**What MR does NOT prove / does NOT build:** see §6.

---

## 2. Code ground truth — BUILT vs what MR adds

Cited so builders extend, never re-plan, what exists. All paths under
`/Users/pablofernandez/Work/nmp`.

**Already built (extend, do not touch semantics):**

- Closed `Lane` vocabulary `crates/nmp-router/src/facts.rs:20` — `Nip65Write,
  Hint, Provenance, UserConfigured, IndexerDiscovery, GroupHost, DmInbox`.
- `RelayDirectory` trait `facts.rs:55` — `write_relays`, `extra_relays`,
  `indexers`, `pinned_relays`, `knows_write_relays` (default `!write_relays()
  .is_empty()`, `facts.rs:85`), `ingest_write_relays` (default no-op,
  `facts.rs:104`). `LiveDirectory` `facts.rs:295` stores `write:
  BTreeMap<PubkeyHex, Vec<LanedRelay>>` + `indexers: Vec<RelayUrl>` and is the
  one live impl; `FixtureDirectory` `facts.rs:166` is the test impl.
- `DiscoveryKinds` `facts.rs:139` — default `{0,3} ∪ 10000..=19999`;
  `is_discovery` `facts.rs:152` (non-empty subset test). Note: kind:10050 is
  already a discovery kind by this range, so a `RelaySource::RelayListKind{10050}`
  can be bootstrapped via the indexer lane with no new discovery-kind change.
- `build_candidates` `crates/nmp-router/src/route.rs:99` — folds indexers into
  each author's per-author candidate list **only for discovery-kind skeletons**;
  content atoms get `write_relays ∪ extra_relays` only. **This is the function
  Unit B narrows.**
- Coverage solver `crates/nmp-router/src/solver.rs:54` — greedy capped k-cover;
  `CoverageInput.candidates` `solver.rs:16` currently counts **whatever is in
  the candidate list** (incl. indexer/extra) toward `k`. `Coverage.shortfall`
  `solver.rs:32` already computes the under-`k` set with typed
  `ShortfallReason::{NoCandidates, FewerCandidatesThanK, CapExhausted}`
  (`solver.rs:43`) — **this is the fallback trigger Unit B reuses.**
- `Router::compile` `crates/nmp-router/src/router.rs:44` — full-recompile-then-
  diff; `k: 2` is hardcoded at `router.rs:81`, `cap` is a compile param.
  Widen-only coalescing via `RuleRegistry` (`coalesce.rs`, `default_widen_only`,
  `register`, `coalesce`); skeleton-stable `SubId` `plan.rs:18`; surgical
  `diff_plans` `plan.rs:82`.
- Read-side typed provenance `route.rs:17` `RouteProvenance{relay, lane,
  covers_authors, route_kind: OutboxSolved|Pinned}`.
- Write outbox `crates/nmp-engine/src/outbox/mod.rs` — `WriteIntent`,
  `WritePayload::{Unsigned, Signed}` (`:37`), `WriteRouting::{AuthorOutbox,
  ToInboxes, PrivateNarrow}` (`:50`), `NarrowOnly<T>` (`:69`, no widen op),
  `WriteStatus` (`:99`, incl. `Routed(BTreeSet<RelayUrl>)` at `:103` and whole-
  intent `Failed(String)` at `:112`).
- Write path in core: `resolve_routes` `crates/nmp-engine/src/core/mod.rs:634`
  — **the flagged `ToInboxes` deviation is at `:654`** (falls back to
  recipients' `write_relays ∪ extra_relays`, documented NOT-correct at `:624`).
  `on_signed` `:540` (Signed → resolve_routes → Routed → PublishEvent per
  relay). `Effect::PublishEvent` `:201`.
- Self-bootstrapping outbox: `sync_discovery` `core/mod.rs:1084` (widen-only,
  **kind:10002-hardcoded** via `NIP65_RELAY_LIST_KIND` const at `:65`);
  `ingest_relay_list_winner` `:1144`; `parse_nip65_write_relays` `:1438`
  (**drops `"read"`-marked entries at `:1449`** — Unit A adds the mirror).
  `on_relay_frame` Event arm `:840` routes a `RelayList` to
  `ingest_relay_list_winner` and bumps `events_by_relay_kind`.
- Ingest-time signature verification **LANDED** (`crates/nmp-transport/src/pool/
  verify.rs`, merge `9220f65`): kind-blind, verify-once-per-id at the frame
  seam; MR assumes verified events at ingest and **does not re-plan it**.
- FFI config `crates/nmp-ffi/src/facade.rs:43` `NmpEngineConfig { store_path,
  indexer_relays }`; `build_directory` `:52` → `LiveDirectory::new(indexers)`.
  **The only relay fact an app supplies today is indexers.**
- Workspace `Cargo.toml`: 10 member crates; **no `nmp-audit`, no `nmp-mod-*`
  yet.** `nmp-bdd` is the existing workspace-level test crate that dev-deps on
  every crate — the structural precedent for `nmp-audit`.

**What MR adds:** the seven groups in `routing-and-ownership.md §6` NEW list,
decomposed into the units below.

---

## 3. Unit decomposition (dependency-ordered, fan-out-able)

Each unit = one PR / one worktree / one builder. Dependencies are explicit.
`∥` marks units that fan out in parallel. Test obligations name the failing-
on-pre-fix falsifier where one exists.

### Dependency graph & the serial spine

```
A ──┬── B ──┬── E ── {F, G} ── I
    │       │
    └── C ──┴── D-read (needs B), D-write (needs C), H (needs C)
```

- **Serial spine (the true critical path): A → B → E → G.**
  A is the additive foundation; B is the one semantic change to proven code
  (the solver-input narrowing); E is the override-primitive types that G's
  `KindClaim.route_policy` embeds; G is the load-bearing ownership audit.
- **Widest parallelism:** after **A** lands, **B** and **C** fork immediately
  (read solver vs write path, different files). After B: **D-read**. After C:
  **D-write, H**. After E: **F** and **G** fork. **I** is the serial tail
  (needs B, C, E, F).
- Minimum spine length is 4 PRs (A,B,E,G); everything else overlaps.

---

### Unit A — Lanes, directory accessors, config surface (additive foundation)

**Scope / files:** `crates/nmp-router/src/facts.rs` (+ re-exports in `lib.rs:42`);
`crates/nmp-engine/src/core/mod.rs` (`parse_nip65_read_relays`);
`crates/nmp-ffi/src/facade.rs` (`NmpEngineConfig`), `crates/nmp-ffi/src/convert.rs`
(lane→string at `:340`).

**NEW:**
- `Lane::{AppRelay, Fallback, Nip65Read}` added to the enum at `facts.rs:20`.
- `RelayDirectory` grows (all additive, default-empty / default-no-op so every
  existing impl keeps compiling — mirror `ingest_write_relays`'s additive
  pattern at `facts.rs:104`):
  - `fn app_relays(&self) -> Vec<RelayUrl> { Vec::new() }`
  - `fn fallback_relays(&self) -> Vec<RelayUrl> { Vec::new() }`
  - `fn read_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> { Vec::new() }`
    (lane `Nip65Read`)
  - `fn ingest_read_relays(&mut self, _author: PubkeyHex, _relays: Vec<LanedRelay>) {}`
- `LiveDirectory` (`facts.rs:295`) grows `read: BTreeMap<PubkeyHex,
  Vec<LanedRelay>>`, `app: Vec<RelayUrl>`, `fallback: Vec<RelayUrl>`; overrides
  the four accessors; `new` gains app/fallback params (or a builder — see owner
  Q5). `FixtureDirectory` gets `with_read`/`with_app`/`with_fallback` builders.
- `parse_nip65_read_relays` in `core/mod.rs` — the mirror of
  `parse_nip65_write_relays` (`:1438`): unmarked `r` tags count as BOTH read
  and write (NIP-65); `"read"`-marked → read only; `"write"`-marked → excluded
  from read. `LiveDirectory` stores both sets from the one kind:10002 winner in
  a single `ingest_relay_list_winner` pass (`:1144` gains an
  `ingest_read_relays` call alongside the existing `ingest_write_relays`).
- `NmpEngineConfig` (`facade.rs:43`) gains `app_relays: Vec<String>`,
  `fallback_relays: Vec<String>` (default empty → Swift `NMPConfig` mirror);
  `build_directory` (`:52`) wires them into `LiveDirectory`.

**Public surface added:** 3 `Lane` variants, 4 trait methods (defaulted), 2
config fields, 3 fixture builders.

**Test obligations (headless):**
- `nip65_unmarked_relay_is_both_read_and_write` — the NIP-65 parse rule.
- `nip65_write_marked_excluded_from_read` / `read_marked_excluded_from_write`.
- `live_directory_stores_read_and_write_from_one_winner`.
- `defaulted_accessors_are_empty_for_fixture_and_dont_break_existing_impls`
  (additive-trait contract, mirrors `default_ingest_write_relays_is_a_no_op...`
  at `facts.rs:470`).
- No pre-fix regression (pure new surface). This is the low-risk base of the
  spine — merge it first, fast.

**Dependencies:** none.

---

### Unit B — Solver-input narrowing + additive lane application + fallback

**The one semantic change to proven code. Highest single-unit risk.**

**Scope / files:** `crates/nmp-router/src/route.rs` (`build_candidates` `:99`),
`crates/nmp-router/src/router.rs` (`compile` `:44`), `solver.rs` (caller
contract only — the greedy algorithm is unchanged).

**NEW / MODIFIED:**
- **`build_candidates` (`route.rs:99`) stops folding indexers into the per-
  author candidate list.** `CoverageInput.candidates` (`solver.rs:16`) becomes
  **the author's own relays only** — `write_relays` + `extra_relays` per owner
  Q3 (see §7; the literal §9-decision-3 reading is that relay *hints* count, so
  `extra_relays` of lane `Hint`/`Provenance` are IN the candidate set that
  counts toward `k`; `IndexerDiscovery`/`AppRelay`/`Fallback` are OUT). The
  solver's `k` semantics are untouched; only its input shrinks.
- **Additive lanes applied OUTSIDE the solve, in `compile` (`router.rs:44`),
  per demand atom:**
  1. Indexer lane — discovery-kind atoms → every `dir.indexers()`, all authors
     (BUILT behavior, now expressed as an unconditional additive route rather
     than a solver candidate). Lane `IndexerDiscovery`.
  2. App lane — every atom → every `dir.app_relays()`, always, all kinds/authors
     **including authorless atoms** (this is the home of the "authorless routing
     lane" known-gap — see §5). Lane `AppRelay`.
  3. Fallback lane — authors whose solved own-relay coverage `< 2` (read from
     `Coverage.shortfall`, `solver.rs:32`) → every `dir.fallback_relays()`,
     **iff `dir.app_relays()` is empty** (appRelay suppresses fallback). Lane
     `Fallback`. `FewerCandidatesThanK`/`NoCandidates` are still REPORTED even
     when fallback tops the author up — fallback is a lane, not coverage.
- These additive routes become `RouteProvenance` entries (`route.rs:17`) with
  the new lanes and feed the same `bag` → `coalesce_with` → `WireReq` pipeline
  (`router.rs:106`) unchanged.

**Public surface added:** none new (internal `compile` behavior + provenance
lanes from A).

**Test obligations (headless — this unit's tests are the §7 mitigation for the
"proven solver's input changed" risk; RE-RUN every existing `solver.rs` and
router property test against narrowed candidates):**
- `solver_counts_only_own_relays_toward_k` — **pre-fix falsifier:** today an
  author with 1 own write relay + a configured indexer reaches `k=2` (indexer
  counted) so no shortfall; post-fix the author is `FewerCandidatesThanK` and
  fallback (if configured) fires. Assert the shortfall now surfaces.
- `app_lane_routes_all_authors_and_authorless_additively_never_toward_k`.
- `fallback_fires_for_under_min_authors_and_is_suppressed_by_apprelay` (both
  branches).
- `indexer_lane_still_discovery_only_never_content_fallback` (regression guard
  on the moved logic — re-assert the `route.rs:241`
  `indexer_candidates_only_for_discovery_kinds` invariant survives the move).
- `additive_relay_roles_union_not_exclusive` (`router.rs:250`) must still pass
  unchanged — a relay that is both an author write relay and an indexer keeps
  both roles.
- A2 two-wave reactive flow (`routing-and-ownership.md §2.6`) still holds:
  kind:0 routes to indexers+appRelay wave 1, then to uX's own relays wave 2
  with skeleton-stable overwriting REQ.

**Dependencies:** A (needs `app_relays`/`fallback_relays` accessors + lanes).

---

### Unit C — Default write policy derived from the event

**Scope / files:** `crates/nmp-engine/src/outbox/mod.rs` (`WriteRouting`),
`crates/nmp-engine/src/core/mod.rs` (`resolve_routes` `:634`, `on_signed` `:540`).

**NEW / MODIFIED:**
- `WriteRouting::Default` variant added (`outbox/mod.rs:50`); becomes the
  default an app-facing publish selects (the publish surface takes event +
  durability, nothing more — no `relays:`). `AuthorOutbox`/`ToInboxes` become
  internals `Default` resolves into; `PrivateNarrow` stays (now minted by Part
  B policies, Unit E/F). Per §2.5 of the spec.
- `resolve_routes` (`core/mod.rs:634`) gains the `Default` arm computing §2.3's
  union from the signed event (kind, author, p-tags all on the event):
  1. author's WRITE-marked relays (all of them — no coverage solve, as today's
     `AuthorOutbox` at `:640`),
  2. p-tagged recipients' **READ-marked** relays (`dir.read_relays`, from A) —
     2 each — **skipped entirely** if the event has >10 p-tags, OR is kind:3,
     OR is kind 10000..=19999 (the broadcast-spam guard; exactly 10 still
     fans out),
  3. `dir.app_relays()` — always, additive,
  4. `dir.fallback_relays()` — author-side only, when author write relays `< 2`
     and no appRelay (recipient-side under-min gets fewer inbox copies, never
     the sender's fallback — the §2.3 trust rule).
- **DELETE the flagged `ToInboxes` write-relay fallback (`resolve_routes`
  `:654`–`:676`)** — replace with `dir.read_relays`. This **closes known-gap
  "DM inbox routing incorrect (M3-D)".**

**Public surface added:** `WriteRouting::Default`.

**Test obligations (headless):**
- `default_write_derives_route_from_event_no_caller_choice`.
- `recipient_inbox_uses_read_marked_relays` — **pre-fix falsifier:** on master
  `ToInboxes` routes to recipients' *write* relays (`:654`); assert Default
  routes to READ relays and diverges when a recipient's read≠write set.
- `eleven_ptags_skips_inbox_fanout_exactly_ten_still_fans_out`.
- `kind3_and_kind1xxxx_never_fan_out_to_ptag_inboxes` (both, regardless of
  p-tag count).
- `ptag_recipient_under_min_gets_fewer_copies_never_senders_fallback`.
- A3 p-tag publish scenario (`§2.6`): route = `{appRelay, r1, r2, r4}`, r2 once
  with both author-write ∪ recipient-inbox provenance.

**Dependencies:** A (needs `read_relays`/`app_relays`/`fallback_relays`).
**Parallel with B.**

---

### Unit D — Relay hints (read counts toward 2-min; write emits hints)

Per `routing-and-ownership.md §9 decision 3`. Splittable into **D-read** and
**D-write** (independent files, independent falsifiers).

**D-read scope:** `crates/nmp-router/src/route.rs`/`facts.rs` — a relay hint
(from an `e`/`p` tag's 3rd position, or from provenance of where an event was
seen) is a first-class candidate that **counts toward the 2-min** (lane `Hint`).
Mechanically this is already half-true: `extra_relays` (lane `Hint`) are in the
candidate set. D-read makes the *source* real — extracting hints from ingested
tags into the directory's per-author extra set, and (owner Q3) confirming hints
count toward `k` under B's narrowed candidates. **Falsifier:**
`relay_hint_counts_toward_two_min` — an author with 1 write relay + 1 tag-hint
reaches `k=2` without fallback.

**D-write scope:** `crates/nmp-engine/src/core/mod.rs` (`on_signed`/publish
composition) + store provenance read. When publishing an event that tags another
event/user, write a relay hint = **the relay where we found the referenced
event** into that tag's 3rd position. Needs the store's per-row provenance
("seen at relay", already a stored field per VISION §4 / ledger #5) surfaced to
the publish composition step. **Falsifier:**
`publish_referencing_event_emits_relay_hint_at_seen_relay`.

**Public surface added:** none app-facing (hint emission is automatic; a hinted
tag is a wire detail).

**Dependencies:** D-read → B; D-write → C (+ store provenance read). **Both
parallel with each other and with E once their parent lands.**

> Note: D-write touches the same publish path as C and the retraction family's
> receipt handling — see §4 collision map.

---

### Unit E — `RoutePolicy` + `RelaySource` + claim-table routing

The override primitive. Per `routing-and-ownership.md §3`. Types + table-driven
dispatch, provable now with a **test-only fixture module** (no real module crate
exists yet).

> **Promoted correction:** schema claims never authorize contextual mutation of a foreign draft; see `protocol-modules-and-composition.md`.

**Scope / files:** `crates/nmp-router/src/` (new module, e.g. `policy.rs`, +
`lib.rs` re-exports) for `RoutePolicy`/`RelaySource`/`AppLanes`/`FailMode`
types and the claim-table routing split; `crates/nmp-engine/src/core/mod.rs`
(write-kind dispatch through the table); `sync_discovery` (`:1084`)
generalization — see the concrete gap below.

**NEW (types, closed vocabularies — extend the enum, never admit a closure):**
- `struct RoutePolicy { read_source: RelaySource, write_source: RelaySource,
  app_lanes: AppLanes, on_empty: FailMode, route_class: RouteClass }`
  (`route_class` from Unit F; E and F can co-develop but F owns the enum).
- `enum RelaySource { Nip65Default, RelayListKind { kind: u16 }, PinnedLane(Lane) }`.
- `enum AppLanes { Apply, Skip }`, `enum FailMode { Closed, OpenToAppLanes }`.
- **Router holds `BTreeMap<u16, RoutePolicy>` derived from the claim table**
  (Unit G) — no hand-maintained per-kind match anywhere.

**Routing behavior:**
- Atom/event classification once, up front: an atom whose `kinds` are all owned
  by one policy routes under that policy; else default. A filter mixing policy-
  owned and default kinds is **split** per-policy before routing (reuse the
  skeleton-grouping machinery at `router.rs:51`; kinds are already a set field;
  widen-only coalesce may re-merge per relay after).
- `AppLanes::Skip` removes appRelay/fallbackRelay from both directions for those
  kinds (a NIP-17 gift-wrap never touches the app relay even though the app
  relay "takes everything" — everything means *default-routed* everything).
- `FailMode`: `Closed` → empty resolved source terminates as `WriteStatus::
  Failed` (`outbox/mod.rs:112`) before any `PublishEvent` (structurally: the
  policy mints a `PrivateNarrow`/`NarrowOnly` empty set, the existing ledger-#6
  path at `resolve_routes:677` fires). `OpenToAppLanes` → empty source re-admits
  app/fallback **for that event only**.
- **`Skip` + `OpenToAppLanes` compose and BOTH fields are required** (no
  defaulting): Skip = "app lanes are not additive routes"; open-on-empty = "app
  lanes are the last-resort route when the source is empty." Different questions.
- Foreign-owned drafts are outside this table even when they are published in a
  protocol context. Only the context contribution for that one intent may pin a
  host; the foreign kind remains under its own/default schema policy.

**Concrete code-reality gap to close (do not gloss):** `RelaySource::
RelayListKind{kind}` (e.g. NIP-17's 10050, drafts' 10013) needs the *recipient's
/ user's* relay-list of that kind discovered. `sync_discovery` (`core/mod.rs:1084`)
is **hardcoded to kind:10002** (`NIP65_RELAY_LIST_KIND`, `:65`). The spec claims
"zero new machinery" because every 1xxxx kind is a `DiscoveryKind` — but the code
only opens a discovery sub for 10002. E must **generalize `sync_discovery` to
open discovery subs for whatever relay-list kinds the linked policies reference**
(a set derived from the claim table, not a hardcoded const). This is a real,
bounded sub-scope of E and a candidate owner-clarification (§7 Q6).

**Public surface added:** `RoutePolicy`, `RelaySource`, `AppLanes`, `FailMode`
(re-exported from `nmp-router`).

**Test obligations (headless, via a test-only fixture module claiming a made-up
kind set):**
- `owned_kinds_route_via_relaylistkind_not_default`.
- `apprelay_skipped_for_skip_policy_even_though_it_takes_everything`.
- `filter_mixing_owned_and_default_kinds_is_split_per_policy`.
- `failclosed_empty_source_terminates_before_any_publish_effect` vs
  `failopen_empty_source_readmits_app_lanes` — the two fail-mode branches.
- `skip_and_opentoapplanes_compose_correctly` (the K3 leak-cell guard, expanded
  in Unit I).
- `relaylistkind_source_bootstraps_via_generalized_sync_discovery`.

**Dependencies:** B and C (E swaps the fact source + lane applicability, running
the same solver/coalesce/write path both must have finished changing).

---

### Unit F — `RouteClass` typed provenance

Per `routing-and-ownership.md §3.3`. Fail-closed-by-construction via a `no
Default` enum.

**Scope / files:** `crates/nmp-router/src/policy.rs` (the enum, co-owned with E),
`crates/nmp-engine/src/outbox/mod.rs` (`WriteStatus::Routed`),
`crates/nmp-engine/src/core/mod.rs` (`on_signed` `:540`, pre-signed path).

**NEW:**
- `enum RouteClass { Automatic, HostPinned, VerifiedPrivateInbox, Manual,
  Imported, Diagnostic }` — **no `Default` impl, no app-reachable constructor**;
  minted only by the routing layer. `Manual`/`Imported` **feature-gated out of
  the app SDK build** (§9 decision 4).
- `WriteStatus::Routed` (`outbox/mod.rs:103`) becomes `Routed(RouteClass,
  BTreeSet<RelayUrl>)` so receipts/diagnostics show the trust regime. **This is
  a breaking change to the `Routed` variant — update `on_signed` (`:578`,
  `:581`) and every match/emit site in one PR (no compat alias — repo rule).**
- **Pre-signed publish (`WritePayload::Signed`, `outbox/mod.rs:39`) routed ONLY
  by the claim table:** owned kind → owner's policy + its class; unowned kind →
  default → `Automatic`. `Manual`/`Imported` are the only classes an operator
  surface can request, and they are absent from the app SDK build. A pre-signed
  kind:1059 with no nip17 module linked → owner-less sensitive kind → `Automatic`
  public (the §9-decision-2 accepted behavior — linking nip17 is what makes it
  safe).

**Test obligations:**
- `routeclass_has_no_default_and_no_public_constructor` (compile-fence — a
  `trybuild`/doc-test that asserts `RouteClass::default()` and struct-literal
  construction don't compile from outside the crate).
- `presigned_giftwrap_without_nip17_routes_automatic_public` — proves the
  accepted §9-2 behavior AND documents loudly that linking nip17 is the fix
  (this is a "documented leak, closed by composition" falsifier, not a bug).
- `presigned_owned_kind_carries_owners_class`.
- `manual_imported_absent_from_sdk_build` (feature-gate assertion).

**Dependencies:** E (the table mints the class) + C (write path). Parallel with G.

---

### Unit G — The kind-ownership boundary + `nmp-audit` (the load-bearing unit)

Detailed in §4 of this plan (its own section — it is the K2 correctness piece).

The audit enumerates exact schema claims. In particular, a future NIP-29 module
must list only NIP-29-defined management/state schemas; a broad 90xx/3900x range
that captures unrelated schemas is a failure, not convenient shorthand.

**Dependencies:** E (RoutePolicy embeds in KindClaim). Parallel with F.

---

### Unit H — Indexer backfill / write-back (§9 decision 1)

**Scope / files:** `crates/nmp-engine/src/core/mod.rs` (`on_relay_frame` Event
arm `:840`, publish composition), coverage/provenance read from
`crates/nmp-store`.

**NEW behavior:** when NMP receives a *newer* event (e.g. a fresher kind:0 /
kind:10002) that the indexer relay it is using did **not** have, contribute that
event *back* to that indexer (republish), keeping the indexer fresh. "Did not
have" is derived from store provenance (the event's `seen` set does not include
that indexer) combined with the fact the indexer was queried for that shape
(coverage/attribution state). Emits a `PublishEvent(indexer, event)` for the
discovery-kind event.

**Test obligations:**
- `newer_event_than_indexer_had_is_republished_back_to_that_indexer` — ingest a
  fresher kind:0 from relay X while the indexer's copy is older/absent; assert a
  write-back `PublishEvent(indexer, ...)`.
- `event_the_indexer_already_served_is_not_written_back` (no echo storm).

**Dependencies:** C (publish path exists). Independent of E/F/G; schedule late.
Touches `on_relay_frame` Event arm — **collides with retraction (see §4).**

---

### Unit I — Decision-table + §2.6 acceptance scenarios (the K3 mitigation, serial tail)

**Scope / files:** router + engine integration tests (`crates/nmp-router/tests/`,
`crates/nmp-engine/tests/`).

**NEW:** the **full decision table enumerated as tests** — every
`(lane config {none|indexer|app|fallback|app+fallback}) × (policy {default |
Skip+Closed | Skip+OpenToAppLanes | Apply+Closed}) × (fail-mode) × (payload
{unsigned | pre-signed}) × (p-tag/kind exclusion {≤10 | >10 | kind:3 | kind:1xxxx})`
cell asserted for the correct route set and `RouteClass`, with the **leak cells
targeted explicitly** (gift-wrap must never reach appRelay; pre-signed 1059 must
never route `Automatic` when nip17 IS linked; fallback must never be suppressed
when it should fire; an under-min author must never be silently unrouted). Plus
A1/A2/A3 from `§2.6` as end-to-end router/engine tests.

**Dependencies:** B, C, E, F. Serial tail — the last unit before MR is claimable.

---

## 4. Unit G in detail — the ownership audit (closes ledger #14)

This is the K2 load-bearing piece. Per `routing-and-ownership.md §4`. Three
enforcement layers, **none of them a lint or a linker-symbol trick.**

### 4.1 The claim — one typed fact per module (types)

In `nmp-router` (or a new tiny `nmp-ownership` types crate — see owner Q7):

```rust
pub struct KindClaim {
    pub owner: ModuleId,                 // &'static str newtype, e.g. "nip17"
    pub scope: KindScope,
    pub exclusive: bool,
    pub route_policy: Option<RoutePolicy>,   // route authority ⊆ ownership,
}                                            // by construction: NO standalone
                                             // "register a policy" API exists.
pub enum KindScope { Kind(u16), Range(RangeInclusive<u16>), Set(&'static [u16]) }
```

A module crate exports `pub fn claims() -> &'static [KindClaim]` — plain const
data, no macro required (`declare_crate_ownership!` may return later as sugar,
never as the mechanism). `Range`/`Set` kill the legacy per-kind repetition.

### 4.2 Three enforcement layers

1. **Engine-construction check (types + fail-fast).** The engine builder takes
   `Vec<ModuleRegistration>`; construction folds all claims into the routing
   table (`BTreeMap<u16, RoutePolicy>`, Unit E) and **returns a typed error**
   (not panic) on any exclusive-scope overlap among *linked* modules. Runs in
   every test that builds an engine (`EngineCore::new`, `core/mod.rs:311` grows
   a claim-registration path) → catches the common case at the first
   `cargo test`. **This is where `EngineCore::new`'s signature changes** —
   coordinate with every construction site: `facade.rs:58`, `nmp-demo/src/
   main.rs:149`, `nmp-bdd/src/world.rs:457`, and the engine test harnesses
   (`diagnostics_headless.rs:32`, `core_headless.rs:77`, `discovery_churn.rs`,
   `self_bootstrap_outbox.rs`). Default (no modules) = empty claim table =
   today's all-`Automatic` behavior, so existing call sites pass an empty
   registration vec.

2. **The static workspace audit (the load-bearing layer) — `nmp-audit`.** A new
   workspace member crate whose sole content is a CI test. Mechanism:
   - **cargo-metadata driven**: at test time, run `cargo metadata` (via the
     `cargo_metadata` crate) to enumerate every workspace member whose name
     matches the module pattern (`nmp-mod-*`). Assert each such crate is a
     **dev-dependency of `nmp-audit`**. A new `nmp-mod-*` crate that forgets to
     enroll appears in metadata but not in `nmp-audit`'s deps → **red build.**
     (This is the legacy audit's key property re-earned without the linker: the
     set of modules is discovered from cargo, not hand-listed.)
   - Collect every enrolled module's `claims()` and assert: **(a)** no
     exclusive-scope overlap **across the whole workspace, including modules no
     app currently links** (`KindScope` intersection: Kind∩Kind, Range∩Range,
     Set∩Set, and cross-variant); **(b)** every `RoutePolicy` is attached to a
     claim whose scope covers it (route authority ⊆ ownership — re-asserted
     across crates even though the struct makes it true by construction);
     **(c)** claimed scopes don't intersect `DiscoveryKinds` (`facts.rs:139`)
     unless the claim sets an explicit `discovery_ack` flag (a module claiming
     1xxxx must consciously interact with indexer semantics).
   - **Failure output names both owners and the overlapping scope** — the legacy
     `NMP-OWNERSHIP-COLLISION` map, minus the linker.
   - `nmp-audit` is added to `Cargo.toml` workspace members; `nmp-bdd` is the
     precedent (it already dev-deps every crate).

3. **The runtime publish gate (table-driven).** At route resolution
   (`resolve_routes` / `on_signed`), one lookup: event kind → claim table.
   Owned-exclusive kind → routed by the owner's policy carrying the owner's
   `RouteClass`; the default policy **refuses** it structurally — `Automatic` is
   simply not the table entry for that kind, and **there is no bypass parameter
   to pass**. Unowned kind → default → `Automatic`. This replaces the legacy
   per-kind `validate_publish_ownership` with the class system: "kind 1059
   requires nip17 provenance" becomes "kind 1059's table entry routes
   `VerifiedPrivateInbox` or fails closed."

**Dropped from legacy (deliberately):** the compile-time linker-symbol collision
(fragile across feature unification/platforms, redundant once layer 2 exists);
the source-surface regex scan (a lint by another name); the hand-maintained
per-kind `match` (the table *is* the claims).

### 4.3 How this closes ledger #14 (falsification tests, in `nmp-audit` + router)

New ledger entry **#14 — Route-override without ownership / ownership
collision**: "a routing override is only representable inside a `KindClaim`;
overlapping exclusive claims are a red build workspace-wide; an owned kind cannot
be routed by the default policy." Falsifiers (each must fail-to-compile or
fail-closed):
- `two_exclusive_claims_on_one_kind_fail_the_audit` — two fixture modules
  claiming an overlapping scope → audit test red.
- `unenrolled_module_crate_is_a_red_build` — a fixture `nmp-mod-*` in metadata
  but not dev-depped → enrollment assertion red.
- `owned_kind_cannot_route_automatic` — attempt to route an owned kind through
  the default policy → no code path (table returns owner policy; assert the
  default arm is unreachable for owned kinds).
- `bare_routepolicy_has_no_registration_api` — compile-fence: there is no
  standalone `register_route_policy`; `Option<RoutePolicy>` lives only inside
  `KindClaim`.

### 4.4 State at THIS milestone

There are **zero real module crates** (`nmp-mod-nip17` etc. are all future). Per
`routing-and-ownership.md §6`, Unit G still lands now ("the audit is cheap and
should not wait for a collision"): the types + construction check + the
`nmp-audit` cargo-metadata harness (enumerates the currently-empty `nmp-mod-*`
set, green) + **test-only fixture modules** inside `nmp-audit`'s own tests
proving overlap/enrollment/route-authority are caught + the ledger #14 entry
with honest CI-proof status (`M-Routing (headless)`: fixture-module falsifiers
green; real-module falsification pending the first `nmp-mod-*`). Do not fake a
real module — the fixtures live in the audit crate's test tree.

---

## 5. Known-gaps & issues that fold in vs stay separate

**Fold IN (MR closes them):**
- **"DM inbox routing incorrect (M3-D)"** (`known-gaps.md`) — the per-pubkey
  read/inbox accessor is Unit A (`read_relays`/`ingest_read_relays`); the
  correct read-marked p-tag fan-out is Unit C (deletes the flagged `:654`
  fallback). **Closed by MR.**
- **Authorless routing lane** — a read atom with no authors is classified
  `Pinned` today (`route.rs:87`) and routes only via `pinned_relays` (→ empty →
  unroutable for a plain `kinds:[X]` global query). Unit B's app lane ("all
  kinds, all authors, always") is its home; a no-author non-discovery read with
  no appRelay routes nowhere, honestly. **Folded into Unit B.**
- **"No time driver for liveness/timeout sweeps"** is **already DESIGN-RESOLVED
  in the retraction family** (`retraction-and-negative-deltas.md §3.3`), NOT MR
  — flagged here only because MR's discovery-sub generalization (Unit E) shares
  `sync_discovery`. MR does not build the time driver.

**Stay SEPARATE:**
- **Decrypt-result feedback path (M3-C, `Effect::RequestDecrypt` no-op at
  `core/mod.rs:193`)** — that is NIP-17 *content reading* (ledger #12 encrypted
  path), not routing. MR routes gift-wraps (kind:1059) as opaque events; it
  never decrypts. Separate follow-up (the nip17 module's content half).
- **AUTH / NIP-42 policy** — `on_relay_frame` defers AUTH (`core/mod.rs:828`,
  `:948`). Writing to an AUTH-gated relay is a transport-handshake concern, not
  a routing-fact concern; the spec does not cover it. **Explicitly out of MR
  scope** (owner Q8 flags whether it should be a sibling milestone).
- **Drafts module (`nmp-mod-drafts`, kind:10013)** — §9 decision 5: spec
  high-level only, **do not build**; capture requirements in a follow-up GitHub
  issue. MR builds the `RelaySource::RelayListKind`/`FailMode::OpenToAppLanes`
  *primitives* drafts will use, and proves them with a fixture module, but
  ships no drafts crate.
- **NIP-17 / NIP-29 module crates** — MR builds the seam (Parts B/C) and proves
  it with test fixtures; the real `nmp-mod-nip17`/`nmp-mod-nip29` crates are the
  forcing-function follow-ups that consume it (and are what turn ledger #14's
  proof status from "fixture" to "real"). NIP-29's claim covers only its exact
  schemas; its `h`/host contribution to foreign drafts uses the separate
  per-intent composition seam described in the promoted canonical contract.

---

## 6. Sequencing vs the retraction family

The shared-file collision map and landing order are in
[`routing-retraction-collision-map.md`](routing-retraction-collision-map.md).

---

## 7. What MR does NOT do, and open owner questions

**MR explicitly does NOT:**
- Build any real protocol-module crate (`nmp-mod-nip17`/`nip29`/`drafts`) — only
  the seam + fixture-proven primitives. Ledger #14's real-module falsification
  stays "pending first `nmp-mod-*`."
- Decrypt anything (gift-wraps route as opaque events; ledger #12 content path
  is separate).
- Handle AUTH / NIP-42 writes to gated relays.
- Build the NIP-40 expiry time-driver (retraction family owns it).
- Re-plan signature verification (landed, `9220f65`).
- Change the two-noun app surface — publish still takes event + durability, read
  still takes a live query; no `relays:` parameter is added anywhere.

The last bullet records the surface at the time this plan was written. It is not
a compatibility freeze: promoted contextual publication may add provisional
typed inputs while preserving live query + write intent as the only operations.

**Open owner questions (surfaced, not resolved — §9 is silent or the code
contradicts spec optimism):**

- **Q3 — do relay hints REALLY count toward the 2-min, and does that mean
  `extra_relays` (lane `Hint`/`Provenance`) count toward `k` in Unit B?** §9
  decision 3 says hints count (superseding §8-3's "only write relays"). Unit B's
  candidate-narrowing must therefore keep `Hint`/`Provenance` extras IN the
  `k`-counting set while excluding indexer/app/fallback. Confirm this reading —
  it is the exact line between "own relays" and "additive lanes," and getting it
  wrong changes when fallback fires.
- **Q5 — `LiveDirectory::new` / `NmpEngineConfig` shape for three lanes.** Add
  `app_relays`/`fallback_relays` as positional `new` params, or move to a
  builder? (Affects every construction site.) Recommend a builder given the
  growing parameter list; confirm.
- **Q6 — `sync_discovery` generalization (Unit E).** The spec says
  `RelaySource::RelayListKind` bootstraps "with zero new machinery" because
  1xxxx is discovery-eligible, but `sync_discovery` (`core/mod.rs:1084`) is
  hardcoded to kind:10002. Confirm the intended design: generalize
  `sync_discovery` to open discovery subs for the set of relay-list kinds the
  linked claim table references (recommended), vs a per-policy discovery hook.
- **Q7 — where do the ownership types live?** `KindClaim`/`KindScope`/
  `RoutePolicy` in `nmp-router`, or a new tiny `nmp-ownership` types crate that
  both `nmp-router` and future `nmp-mod-*` crates depend on (avoids modules
  depending on the whole router)? Recommend a small shared types crate; confirm.
- **Q8 — is AUTH a sibling milestone?** Out of MR scope, but writing to
  AUTH-gated relays will block real-world publish; confirm it is a separate
  tracked milestone, not silently dropped.
- **Q2-restated (from spec §8-2, §9-2 accepted (a)):** a pre-signed kind:1059
  with no nip17 linked routes `Automatic` public. §9 resolved this as accept +
  loud docs. Unit F builds exactly that. Flagging only so the "documented leak"
  falsifier (`presigned_giftwrap_without_nip17_routes_automatic_public`) is not
  mistaken for a bug at review time.

### 7.1 Resolutions (orchestrator, 2026-07-11 — owner may override)

None of the above is a genuine fork: each is already settled by spec §9 / the
owner's prior words, or has a doctrine-clear answer. Resolved so the milestone
can fan out; recorded here for override.

- **Q3 — RESOLVED YES (settled by §9-3 / owner's own words).** Relay hints count
  toward the 2-relay minimum. Mechanics for Unit B: the `k`-counting candidate
  set is **an author's own relays only** — `Nip65Write` ∪ `Hint` ∪ `Provenance`
  (write-hints AND read-hints, per §9-3). The **additive lanes** — `IndexerDiscovery`,
  `AppRelay`, `Fallback` — are applied *after* the solver and **never count toward
  `k`** (else `Fallback` could never fire — an app relay would always satisfy the
  minimum). This is the exact "own relays vs additive lanes" line; Unit B's
  narrowing must encode it and a falsifier must pin it (`app_relay_does_not_suppress_fallback_shortfall`,
  `hint_counts_toward_k`).
- **Q5 — RESOLVED: builder.** `LiveDirectory` / `NmpEngineConfig` move to a
  builder rather than positional `new` params — the lane list is still growing
  (this milestone adds three), and positional params would churn every
  construction site again on the next lane. One edit now, none later.
- **Q6 — RESOLVED: generalize `sync_discovery`.** It opens discovery subs for the
  **set of relay-list kinds referenced by linked claim-table `RoutePolicy`s**
  (kind:10002 becomes one member of that set, not a hardcoded const), which is
  the spec's stated "`RelayListKind` bootstraps with zero new machinery" design.
  Not a per-policy discovery hook. The `NIP65_RELAY_LIST_KIND` const at
  `core/mod.rs:65` becomes a default member of a queried set.
- **Q7 — RESOLVED: new `nmp-ownership` types crate.** `KindClaim` / `KindScope` /
  `RoutePolicy` / `RelaySource` / `RouteClass` live in a small, dependency-light
  `nmp-ownership` crate that both `nmp-router` and every future `nmp-mod-*`
  depend on. This is **load-bearing for the modularity doctrine** (a protocol
  module declares its kind-claim + route policy WITHOUT linking the whole router
  — the "opt-in per-NIP, a minimal app links zero non-primitive code" north
  star). Putting these types in `nmp-router` would force every module to depend
  on the router; rejected for that reason. `nmp-audit` (Unit G) dev-deps every
  crate and reads their declared claims — same structural precedent as `nmp-bdd`.
- **Q8 — CONFIRMED separate, tracked (not dropped).** AUTH / NIP-42 (the owner's
  policy: auto-auth for relays the user likes; ask the user before authing to
  unknown relays) is its own sibling milestone. Writing to AUTH-gated relays will
  block some real-world publishes, so it is explicitly on the queue, filed, not
  silently deferred.

---

## 8. Summary

- **Milestone:** MR — Routing & Ownership. Thesis: every relay decision is
  compiler output from lane facts + schema-ownership claims or typed per-intent
  context, core is NIP-blind, and the
  ownership boundary is enforced by cargo-metadata + types, not a lint. Kills:
  K1 (override vocabulary not general / core goes NIP-aware), K2 (ownership
  can't be structural), K3 (composition-matrix leak cell).
- **9 units:** A (lanes/accessors/config) · B (solver narrowing + additive lanes
  + fallback) · C (default write policy from event) · D (relay hints, split
  read/write) · E (RoutePolicy/RelaySource/claim-table) · F (RouteClass) · G
  (KindClaim + `nmp-audit`) · H (indexer backfill) · I (decision-table + §2.6).
- **Serial spine (4 PRs):** A → B → E → G. C forks off A parallel to B; F and G
  fork off E; D/H hang off B/C; I is the tail.
- **Load-bearing unit:** G — cargo-metadata `nmp-audit` crate + `KindClaim`
  types + construction-time overlap error + runtime table gate; closes ledger
  #14 via four fixture-module falsifiers (real-module falsification pending the
  first `nmp-mod-*`).
- **Folds in:** DM inbox read-relay accessor + correct inbox fan-out (closes
  M3-D); authorless lane (→ app lane). **Stays separate:** decrypt path, AUTH,
  the real nip17/nip29/drafts crates, the contextual-publication seam, the
  retraction time-driver.
- **Collision risk:** retraction family shares `core/mod.rs` (`on_relay_frame`
  Event arm, `EngineCore`/`new`, receipt terminals) — land crate-local units
  (A/B, store-door) in parallel, then serialize or share-worktree the
  `core/mod.rs` seam; `EngineCore::new` signature is the top conflict point.
- **Owner questions:** Q3 (hints count toward k — pins Unit B), Q5 (config
  shape), Q6 (`sync_discovery` generalization — spec-vs-code gap), Q7 (ownership
  types crate), Q8 (AUTH as sibling milestone).
