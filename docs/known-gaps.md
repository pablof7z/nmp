# Known gaps & deferred follow-ups

Honest running list of things built-but-incomplete or deliberately deferred, so nothing hides. Each says who flagged it, why it's deferred, and when it must be closed. This is a truth-anchor companion to the bug-class ledger.

## Load-bearing for M5 (the falsifier app) — must close before M5 claims pass

- **`RelayDirectory` has no reactive update path (found dogfooding the Rust demo).** It is boxed into `EngineCore` at `spawn` with no way to update it afterward, and no live (non-fixture) implementation exists — an app must pre-resolve all NIP-65 relays before spawn (as `nmp-demo/src/bootstrap.rs` does). The falsifier's "pick relays from your follows' kind:10002" mode needs relay facts to update *reactively* as follows and their 10002s change. **Fix:** a live, updatable `RelayDirectory` fed by the engine's own kind:10002 ingestion (itself expressible as a `Derived` binding). Needed for M5's relay-picker mode.


- **Multi-account signer switching is NOT wired (M3-C).** `set_active_pubkey` re-roots the *read* graph (M1-proven identity re-root), but the signer is a fixed constructor argument to `EngineThread::spawn`. So publishing *as a different account* after a switch has no path. The falsifier's #1 requirement is multi-nsec login + switch, which includes writing as the switched-to account. **Fix:** the engine must hold a signer per account (or a signer registry) and `set_active_signer`/account-switch must select both the reactive pubkey and the active signing capability. Owner: M4/M5 boundary work. Until then, "account switching" is proven for reads only.

## Real but non-blocking for the falsifier (feeds, not DMs)

- **DM inbox routing incorrect (M3-D).** `WriteRouting::ToInboxes` needs each recipient's kind:10050 / NIP-65 *read* relays, but `RelayDirectory` (nmp-router) exposes only `write_relays`/`extra_relays`/`pinned_relays`. D fell back to the union of recipients' write relays and **documented it inline as NOT correct routing** rather than shipping silent wrongness. **Fix:** add an additive per-pubkey inbox/read-relay accessor to `RelayDirectory`. Needed for NIP-17 DMs; the falsifier does feeds, so deferred.

- **Decrypt-result feedback path missing (M3-C, plan §8 item 2).** `Effect::RequestDecrypt` is an explicit no-op; there is no `EngineMsg` to feed a decrypt result back into ingest. Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12 encrypted-content path). Deferred with E/negentropy still open; not on the falsifier's feed path.

- **Reconnect loses negentropy-first temporarily (M3-E).** On reconnect, subs previously routed negentropy-first are replayed as plain REQ (safe, correct — just less efficient) until the next real demand change re-routes them. A "reroute negentropy-first on reconnect" refinement was deferred. Perf, not correctness.

- **No time driver for liveness/timeout sweeps (M3-E).** The 30s liveness-deadline sweep (`tick()`) is real and unit-tested against a synthetic clock, but nothing drives it periodically in the live runtime — D8 forbids a fixed-rate poll-loop timer thread, and `Handle` has no `tick` verb. A **D8-compliant** driver is needed (event-driven: a sleep-until-next-deadline that wakes on the real deadline, or a mio timeout on the engine loop — NOT fixed-rate polling). The falsifier's feed path works without it; needs a design decision at M4.

## Security hardening deferred

- **Secret zeroization not harvested (M3-A3, old-repo V-55).** `nmp-signer` holds `nostr::Keys` without the old repo's zeroize/raw-bytes secret-residency hardening. Reasonable to defer, but a real security property to restore before any v2 ship. Owner: post-M3 security pass.

## Process / tooling

- **CI runs on push but is not proven to gate a PR** (no branch protection configured). The workflow exists (`.github/workflows/ci.yml`); enabling required-status-checks is an owner/settings action, not code.
