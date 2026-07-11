# Known gaps & deferred follow-ups

Honest running list of things built-but-incomplete or deliberately deferred, so nothing hides. Each says who flagged it, why it's deferred, and when it must be closed. This is a truth-anchor companion to the bug-class ledger.

## Load-bearing for M5 (the falsifier app) — must close before M5 claims pass

- **~~`RelayDirectory` has no reactive update path~~ CLOSED (self-bootstrapping outbox).** `nmp_router::LiveDirectory` is a live, updatable `RelayDirectory` (write relays start empty, fed at runtime via `RelayDirectory::ingest_write_relays`); `nmp_engine::core::EngineCore::sync_discovery` watches active content demand for authors whose write relays are still unknown and opens an internal kind:10002 discovery subscription against the configured indexers for exactly them (reusing the ordinary resolver subscribe/unsubscribe machinery, not a parallel subscription system) -- when that kind:10002 lands, the winning event is re-read from the store and fed into the directory, and the very same recompile re-routes that author's content atoms to their real write relay. `nmp-demo`'s two-phase `bootstrap.rs`/`BootstrapDirectory` are deleted; the CLI now configures only two indexer relays and gets real notes with the engine doing discovery. `nmp-ffi`'s `NmpEngineConfig`/Swift `NMPConfig` lost the `write_relays`/`writeRelays` field for the same reason -- an app supplies indexers only. Headless proof: `nmp-engine/tests/self_bootstrap_outbox.rs`. Live proof: `nmp-demo` against real relays, and `Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`.

- **Publish payload is unsigned-only across FFI (M4).** The `nmp-ffi` publish path accepts unsigned intents only; pre-signed verbatim publish isn't exposed yet. Fine for the falsifier's compose flow; revisit if a Swift caller needs to publish an already-signed event.

## Real but non-blocking for the falsifier (feeds, not DMs)

- **DM inbox routing incorrect (M3-D).** `WriteRouting::ToInboxes` needs each recipient's kind:10050 / NIP-65 *read* relays, but `RelayDirectory` (nmp-router) exposes only `write_relays`/`extra_relays`/`pinned_relays`. D fell back to the union of recipients' write relays and **documented it inline as NOT correct routing** rather than shipping silent wrongness. **Fix:** add an additive per-pubkey inbox/read-relay accessor to `RelayDirectory`. Needed for NIP-17 DMs; the falsifier does feeds, so deferred.

- **Decrypt-result feedback path missing (M3-C, plan §8 item 2).** `Effect::RequestDecrypt` is an explicit no-op; there is no `EngineMsg` to feed a decrypt result back into ingest. Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12 encrypted-content path). Deferred with E/negentropy still open; not on the falsifier's feed path.

- **Reconnect loses negentropy-first temporarily (M3-E).** On reconnect, subs previously routed negentropy-first are replayed as plain REQ (safe, correct — just less efficient) until the next real demand change re-routes them. A "reroute negentropy-first on reconnect" refinement was deferred. Perf, not correctness.

- **No time driver for liveness/timeout sweeps (M3-E).** The 30s liveness-deadline sweep (`tick()`) is real and unit-tested against a synthetic clock, but nothing drives it periodically in the live runtime — D8 forbids a fixed-rate poll-loop timer thread, and `Handle` has no `tick` verb. A **D8-compliant** driver is needed (event-driven: a sleep-until-next-deadline that wakes on the real deadline, or a mio timeout on the engine loop — NOT fixed-rate polling). The falsifier's feed path works without it; needs a design decision at M4.

## Security hardening deferred

- **Secret zeroization not harvested (M3-A3, old-repo V-55).** `nmp-signer` holds `nostr::Keys` without the old repo's zeroize/raw-bytes secret-residency hardening. Reasonable to defer, but a real security property to restore before any v2 ship. Owner: post-M3 security pass.

## Process / tooling

- **CI runs on push but is not proven to gate a PR** (no branch protection configured). The workflow exists (`.github/workflows/ci.yml`); enabling required-status-checks is an owner/settings action, not code.
