# M3 independent verification — store + transport + outbox + negentropy + runtime

- **Date:** 2026-07-11
- **Reviewer role:** independent skeptic (review-only, no code changed).
- **Reproduced:** `cargo test --workspace -- --test-threads=1` → **170 passed, 0 failed, 1 ignored**, process EXITS cleanly (no hang). The 1 ignored is a `///` doctest in `nmp-engine/src/core/mod.rs` (a `` ```ignore `` code sketch), not a suppressed failure. `cargo clippy --workspace --all-targets -- -D warnings` → **exit 0, zero warnings**. The reported "170 green" is accurate and independently confirmed.

## Verdict: VERIFIED-WITH-NITS

Offline cold-start authority is **genuinely earned**, the reducer is a genuinely pure sync core, and every load-bearing ledger mechanism (#5–#9) is backed by a real falsifier that would go red if the property eroded. The nits are (a) the headline capstone test is weaker than the headless suite that actually proves the property, and (b) two honestly-disclosed runtime gaps (no `Tick` driver; account-switch read-only). None block M4/M5 provided the gaps stay tracked.

---

## Per-item findings

### 1. Cold-start offline authority — PASS (with a test-strength nit)
`integration_capstone.rs::watermark_cold_start_offline` is structurally honest:
- Phase 2 genuinely has **no relay**: `relay.shutdown()` (line 203) runs before the phase-2 engine spawns; the dead port is retried at a 3600 s interval so nothing reconnects. Zero network.
- It opens a **fresh engine on the persisted redb file** (`RedbStore::open(&db_path)` line 207, new scope after phase-1 `join()`), not leftover in-memory state.
- `CompleteUpTo` is read from the **persisted watermark**: `coverage_query.rs:70` calls `store.get_coverage(key, relay)` and returns `Unknown` on `None` (`:77`). Verified in code — it is never derived from row-emptiness. The `b` control (no coverage row → `Unknown`, same offline engine) rules out "everything cached = complete."
- The phase-2 `a`→`CompleteUpTo` read from a reopened file is itself the proof that the coverage row **survived redb close/reopen** (redb `record_coverage` is a committed `begin_write`/`commit`, `redb_store.rs:228-247`).

**Nit (not a fail):** the capstone's own observations (`a`: rows+Complete; `b`: no-rows+Unknown) are *congruent with* a trivial "non-empty = complete" bug — that specific test would pass under the bug too. What actually falsifies the bug lives in `core_headless.rs`: `eose_records_coverage_watermark_and_non_eose_does_not` (a bare EVENT leaves `get_coverage == None`; only EOSE records — presence ≠ coverage) and `query_reads_complete_up_to_only_when_every_covering_relay_is_proven` (**zero rows + `CompleteUpTo` after both EOSEs** — the true authoritative-empty case). With those, the mechanism is fully proven; the capstone's unique contribution (redb persistence roundtrip) is real.

### 2. Coverage attribution correctness — PASS
`attribution.rs::attribute_eose` implements the ruling exactly: intersection over **all outstanding snapshots** (`:176-187`), `limited` poisons unconditionally (`:170`), oldest popped after (`:195`). Falsifier `eose_overwrite_race_credits_only_the_intersection` proves a straggler EOSE with both snapshots outstanding credits `{a}` but **not** `e` (only in the newer snapshot). `limited_fetch_never_records_coverage` proves limit-poisoning. NEG credit is deferred: `finish_neg_session` (`core/mod.rs:1022`) routes a backfill REQ and credits only on that backfill's EOSE via `pending_neg_credit` (`:781`), so an atom is never credited before the backfilled events are ingested (EVENT precedes EOSE, NIP-01). I could not construct a sequence that credits an atom the wire never proved complete.

### 3. ProbedRelay gate — PASS (minor structural nit)
`Effect::NegOpen`/`open_neg_session` require a `ProbedRelay`; the only constructors are `Prober::probed` (cached `Supported`) and `Prober::on_neg_msg` (`negentropy/mod.rs:145,193`). `recompile` reaches negentropy only via `self.prober.probed(relay)` returning `Some` (`core/mod.rs:877`). Tests cover unprobed→REQ, probed-broad→NegOpen, limited→REQ, NEG-ERR→Unsupported→REQ, plus a source grep-guard that `core/mod.rs` never spells `ProbedRelay(`. **Nit:** the inner field is `pub(crate)`, so a *future in-crate* edit could forge one; unforgeable at the public (app) boundary, which is what ledger #8 requires. Fully private would be strictly stronger.

### 4. Private-narrow (#6) + durability (#9) — PASS
`NarrowOnly<T>` (`outbox/mod.rs:69`) exposes only `new`/`is_empty`/`iter` — no widen/insert/extend/union. `resolve_routes` `PrivateNarrow` with empty set → `Err` (fail-closed, `core/mod.rs:580`), never consults the directory, never falls to public. `private_route_fails_closed` proves no `PublishEvent` + typed `Failed`. Durable path returns a `Receipt`/`Receiver<WriteStatus>` stream (`Handle::publish` `:595`), never `bool`/`()`; `write_ack_per_relay` and `enqueue_is_not_converged` prove per-relay `Acked`/`Rejected`/`GaveUp` terminals and `Accepted`-first. Nowhere on the durable path is a bool/void success.

### 5. Architecture — no runtime leaks — PASS
`Handle` (`runtime/mod.rs:546`) is `Clone`, carries only `Sender<Cmd>` (Cmd private), exposes exactly subscribe/unsubscribe/set_active_pubkey/publish/shutdown returning plain `std::sync::mpsc::Receiver` — no tokio/mio/Pool types cross the boundary (grep-guarded by `handle_surface_is_exactly_four_verbs_plus_shutdown`). `EngineCore::handle` does no I/O (operates on store + returns `Vec<Effect>`); the Pool is constructed *inside* the spawn closure. **D8 holds:** engine loop and pool-bridge loop are blocking `recv()`; `SignerOp::Pending` is a single blocking `recv` on a throwaway thread; transport is `mio` `Poll::poll(timeout)` (event-driven wait on socket/waker/keepalive deadline), **not** a fixed-rate sleep loop. No `thread::sleep` poll loops anywhere in engine/transport/signer/store.

### 6. Hollow-test / weakened-assertion sweep — essentially clean
No `#[ignore]` on any real test (only the doctest sketch). Headless falsifiers assert precise structural facts, not trivialities. Minor observations, none hollow:
- Capstone `watermark_cold_start_offline` — congruent-with-trivial-bug weakness (item 1); mechanism proven by the headless suite, so not hollow overall.
- `negentropy_live` has a single (compound) assert but it is a real end-to-end: b's post is seeded **only** in the relay, discovered via reconciliation, and asserted to surface AND read `CompleteUpTo`. It relies on a 300 ms `tokio::time::sleep` for probe settling — a test-harness wait (theoretical flake if a probe ever exceeds 300 ms over loopback), not an engine poll.
- `handle_surface`/`core_never_constructs_a_probed_relay` are source-scan grep-guards, not type-level — but that is the plan's own named test-14 idiom.

### 7. Known-gaps honesty — PASS, complete
`known-gaps.md` discloses every material hole I found: multi-account signer switch is **read-only** (`set_active_pubkey` re-roots the read graph; signer is a fixed spawn arg — confirmed, no `set_active_signer` path exists); `ToInboxes` DM routing falls back to write-relays and is inline-flagged wrong (`core/mod.rs:527`); `RequestDecrypt` is an explicit no-op with no feedback `EngineMsg` (`runtime/mod.rs:374`); **no `Tick` driver** in the runtime so the 30 s negentropy liveness sweep never fires in production (real, disclosed — the fallback is only reachable via NEG-ERR/malformed-payload live, or `tick()` in tests); zeroize deferred; CI not proven to gate PRs. I found no *undisclosed* gap.

---

## Hollow-green tests / undisclosed gaps
- **Undisclosed gaps:** none found.
- **Hollow tests:** none that are hollow on their own merits. One strength-nit: the flagship capstone cold-start assertion is weaker than advertised (passes under a "non-empty=complete" implementation); the property is genuinely secured by the two headless coverage tests instead. Worth strengthening the capstone with an authoritative-empty offline case, but not blocking.

## What the owner must know before M4/M5
1. **The `Tick` driver gap is on the M4 critical path.** The liveness sweep + any future live-tail watermark advancement need a D8-compliant event-driven timer; until then a negentropy session that goes silent (no NEG-ERR) hangs until reconnect. Disclosed, but M4 must design the driver.
2. **Account-switching is reads-only.** M5's falsifier requires publishing as a switched-to account — there is no signer-per-account path yet. Load-bearing, disclosed.
3. **Coverage is earned only by negentropy or unlimited/window-walked REQ** — a `limit:N` initial fetch never earns a watermark (by design, ruling §3). Ensure M4 feed surfaces don't expect coverage from limited fetches.
4. Consider strengthening the capstone with an authoritative-empty offline case and making `ProbedRelay`'s field fully private.
