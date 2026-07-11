# Known gaps & deferred follow-ups

Honest running list of things built-but-incomplete or deliberately deferred, so nothing hides. Each says who flagged it, why it's deferred, and when it must be closed. This is a truth-anchor companion to the bug-class ledger.

## Load-bearing for M5 (the falsifier app) — must close before M5 claims pass

- **Multi-account signer switching is NOT wired (M3-C).** `set_active_pubkey` re-roots the *read* graph (M1-proven identity re-root), but the signer is a fixed constructor argument to `EngineThread::spawn`. So publishing *as a different account* after a switch has no path. The falsifier's #1 requirement is multi-nsec login + switch, which includes writing as the switched-to account. **Fix:** the engine must hold a signer per account (or a signer registry) and `set_active_signer`/account-switch must select both the reactive pubkey and the active signing capability. Owner: M4/M5 boundary work. Until then, "account switching" is proven for reads only.

## Real but non-blocking for the falsifier (feeds, not DMs)

- **DM inbox routing incorrect (M3-D).** `WriteRouting::ToInboxes` needs each recipient's kind:10050 / NIP-65 *read* relays, but `RelayDirectory` (nmp-router) exposes only `write_relays`/`extra_relays`/`pinned_relays`. D fell back to the union of recipients' write relays and **documented it inline as NOT correct routing** rather than shipping silent wrongness. **Fix:** add an additive per-pubkey inbox/read-relay accessor to `RelayDirectory`. Needed for NIP-17 DMs; the falsifier does feeds, so deferred.

- **Decrypt-result feedback path missing (M3-C, plan §8 item 2).** `Effect::RequestDecrypt` is an explicit no-op; there is no `EngineMsg` to feed a decrypt result back into ingest. Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12 encrypted-content path). Deferred with E/negentropy still open; not on the falsifier's feed path.

## Security hardening deferred

- **Secret zeroization not harvested (M3-A3, old-repo V-55).** `nmp-signer` holds `nostr::Keys` without the old repo's zeroize/raw-bytes secret-residency hardening. Reasonable to defer, but a real security property to restore before any v2 ship. Owner: post-M3 security pass.

## Process / tooling

- **CI runs on push but is not proven to gate a PR** (no branch protection configured). The workflow exists (`.github/workflows/ci.yml`); enabling required-status-checks is an owner/settings action, not code.
