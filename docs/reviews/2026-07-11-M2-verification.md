# M2 (compiler / router + coalescing) — independent verification

- **Date:** 2026-07-11
- **Reviewer:** independent (Opus), read-only
- **Subject:** M2 milestone per `docs/plans/M2-compiler-router-plan.md`; crate `nmp-router`
- **Gate type:** running gate (property + differential), NOT a thesis gate

## Build / test reproduction (run myself)

- `cargo test --workspace` → **91 passed, 0 failed** (15 result lines). Matches builder's claim exactly.
- `cargo clippy --workspace --all-targets -- -D warnings` → **clean** (exit 0).
- `kill_measurement` re-run with `--nocapture` — numbers below are the live output, not prose.

---

## Item 1 — The kill measurement (test 16) is HONEST → **PASS**

`nmp-router/tests/kill_measurement.rs`. 300 authors (realistic follow count), 15-relay pool, 3 popular "big" relays every author's first write relay clusters on, second write relay spread coprime (`step=7`) across the 12 small relays. `RelayLimits::default()` = `max_subs_per_relay:20`, `max_filter_authors:1000` (`facts.rs:75-83`) — realistic, not inflated (v1 evidence: relays cap concurrent subs ~20, accept large author arrays ~1k).

Live measurement:
- **Dedup-only floor:** relay0/1/2 = `wire_sub_count=100` each (> limit 20) → the floor genuinely blows the sub limit (expected, printed honestly, `kill_measurement.rs:116-123`).
- **With AuthorUnion:** every relay `wire_sub_count=1`, `max_filter_authors=100` (limit 1000). `KILL VERDICT: fired=false`. Total subs 600 → 15 (strict-improvement assert, line 155-158, is real).

Measurement correctness: `measure()` (lines 65-84) reads POST-coalescing `router.plan().reqs` — per-relay `reqs.len()` for sub-count and `filter.authors.len()` for author-count. Correct. The "did not fire" verdict is **genuinely earned**: the demand is realistic and the passing margin (100 vs 1000 authors) is real, not manufactured.

**One honest caveat (not rigging):** the falsifier is a *single skeleton* (all kind:1). With one skeleton, AuthorUnion mechanically yields exactly 1 sub/relay at any scale, so the *sub-count* dimension of the kill is structurally unblowable by this demand — only the *filter-author-count* dimension is actually stressed (and passes with 10× margin). This is defensible: a "my-follows feed" IS a single kind:1 skeleton, i.e. the canonical outbox scenario. A multi-skeleton demand would exercise per-relay sub-count, but that is a different falsifier. Worth the owner knowing the sub-count assertion carries slack here.

## Item 2 — Widen-only holds + differential oracle is real → **PASS**

`coalesce.rs`: `AuthorUnion::try_merge` unions author sets only when `same_except_authors` (every other field equal) — trivially widening. `coalesce_with` (lines 183-228) does hash-dedup then fixed-point pairwise merge; the provenance-threading variant is defined in terms of the same rule set as `coalesce`, so they cannot diverge.

Property test `merge_rule_widens_author_union` (`coalescing.rs:27-66`): checks `matches(m) ⊇ matches(a) ∪ matches(b)` via real `nostr::Filter::match_event` (not hand-rolled). Alphabet is non-trivial — 4-key author pool, 3 kinds, up to 6 generated events drawn from the pool so collisions are frequent. `KindUnion` covered symmetrically. Direction of the ⊇ check is correct (lines 59-63).

Differential oracle (`differential_oracle.rs`) wires the **real resolver** (`nmp_resolver::testkit::Harness`, a genuine "my follows" reactive fan-out, `demand.len() == follows+1` asserted). Path A (dedup-only) vs Path B (`default_widen_only`) over identical facts + a per-relay event store; both re-filter to each consumer atom; asserts `delivered_a == delivered_b`, non-empty delivery, noise never delivered, and **`total_reqs_b < total_reqs_a`** (lines 186-191) — so path B provably coalesces and is not a no-op. Cannot pass trivially.

## Item 3 — Solver respects 2-min AND cap under adversary → **PASS**

`solver.rs` greedy capped k-cover. Cap invariant is airtight: the loop checks `selected.len() >= cap` at the top (line 76) and adds exactly one relay per iteration → `|selected| ≤ cap` always; never an accumulated union. Ceiling clamps `k` to each author's distinct-candidate count (lines 64-71); shortfall reported against the *original* `k` with correctly-discriminated reasons (`NoCandidates` / `FewerCandidatesThanK` / `CapExhausted`, lines 136-159). Deterministic lexicographic tiebreak (line 108).

Adversary reasoning I tried to break it with:
- One prolific author (50 relays): its relays each score 1 (cover only itself), so relays covering more needy authors win first; author capped at exactly k=2 (test 4, confirmed). One author cannot blow the cap.
- Disjoint mailboxes, cap<2N: fills to cap, remainder = `CapExhausted`, `|selected|==cap` (test 3). Reports shortfall rather than violating either bound. ✓
- Author with 1 / 0 relays: clamps / `NoCandidates`, content atom never indexer-filled (tests 5, 6). ✓

**Minor design note (not a defect):** the cap is applied *per skeleton-solve* (`router.rs:72-77`), not globally across skeletons. With multiple distinct skeletons total plan relays could exceed a single skeleton's `cap`. The plan calls it a "global fan-out cap"; per-query-shape bounding is defensible and does not affect the kill, but the wording is looser than the implementation.

---

## Nits

- **nit-1 (clippy hardening) — PASS.** Scoped `clippy.toml` in *both* `nmp-resolver` and `nmp-router` bans `nostr::Kind::as_u16` (+ forward-hardening `as_u64` with `allow-invalid`). The single legit read is `eval.rs:66` `event.kind.as_u16() // KIND-VALUE-READ:` inside a one-line helper carrying `#[allow(clippy::disallowed_methods)]` (line 64). Scanner `no_kind_branches.rs::kind_value_reads_are_confined_to_the_one_annotated_projection_site` scans both crates' `src/`, bans `as_u16(`/`as_u64(`/bare `.kind` unless the `KIND-VALUE-READ` marker is on the line, and asserts it is the only site. Both layers (clippy real gate + scanner tripwire) present as described; workspace clippy is green with the ban active.
- **nit-2 (Drop close-delta) — PASS.** `drop_delta.rs` is genuinely end-to-end: subscribe → compile opens REQs (asserts non-empty) → `drop(handle)` → `h.poll_pending_drops()` asserts the resolver's own delta surfaces the atom's Close → recompile asserts `WireOp::Close` for **every** previously-opened sub-id and an empty plan. Real, not narrated.

## Hollow-green audit

I actively looked for weakened / trivially-true assertions. **None found.** Every contract test (1-20) asserts a non-trivial, load-bearing fact: lane provenance, per-author ≥2 coverage, cap+shortfall discrimination, exact-dedup counts, AuthorUnion author-set equality, surgical-diff relay sets, reverse-coverage/lane counts, NIP-29 group-host pinning. The widen property tests are guards over rules that widen by construction, but they run against real `match_event`, so they would catch a regression that broke the construction. The differential oracle and kill both carry anti-no-op assertions (`total_reqs_b < total_reqs_a`; strict sub-count improvement). I state this confidently: the green is earned, not hollow.

---

## Verdict: **VERIFIED-WITH-NITS**

The three load-bearing claims are real and independently reproduced (91/91 green, clippy clean). The M2 kill "did not fire" is genuinely earned on a realistic falsifier with a real passing margin. The two nits are the only qualifiers, and neither is a defect:
1. The kill's single-skeleton demand makes the *sub-count* dimension structurally unblowable; only *filter-author-count* is truly stressed (passes 100 vs 1000). Representative of the real follows feed, but the owner should know the sub-count assertion has slack — a multi-skeleton falsifier would be a stronger future probe.
2. The fan-out cap is per-skeleton, not global across skeletons, despite "global" wording in the plan.

No correctness holes. Widen-only holds, local re-filter makes coalescing non-load-bearing, the solver honors both bounds and reports shortfall rather than violating either. Ship.
