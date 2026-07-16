# NMP

**An embeddable Nostr client engine. You bring the app; NMP owns the network.**

A Rust core with Swift and Kotlin SDKs that packages the hard Nostr client machinery — relay routing, outbox discovery, canonical state, signing, durable publishing — behind a small surface you *call*. Not a framework you live inside.

[![CI](https://github.com/pablof7z/nmp/actions/workflows/ci.yml/badge.svg)](https://github.com/pablof7z/nmp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<p align="center">
  <img src="docs/screenshots/m5-02-feed.jpg" alt="A live feed in NMP's SwiftUI falsifier app" width="220">
  <img src="docs/screenshots/m5-05-relays.jpg" alt="Relay evidence in NMP's SwiftUI falsifier app" width="220">
</p>

<p align="center"><sub>An ordinary SwiftUI app backed by NMP. The app owns the screens; NMP owns the live relay work behind them.</sub></p>

---

## Why this is cool

Nostr's wire protocol is small. A *dependable local view* is not.

- Every serious app re-implements the same distributed plumbing: relay discovery, outbox routing, subscription repair, dedup, replaceable-event rules, deletion, expiry, retry, and "what did the network actually prove?"
- Most implement it **badly** — silent truncation, lost subscriptions, stale replaceable events, fake "synced" booleans.
- NMP concentrates that machinery in **one embeddable engine** with the bad behaviors ruled out at the boundary.
- **A library you call, not a framework you inherit.** Your app keeps its own state model, navigation, identity UX, and UI. NMP never becomes your container or reducer.

## Two nouns

Everything is expressed as one of two things:

- **A live query** — a declarative demand ("these authors' notes"). NMP keeps the local view current, repairs relay work when inputs change, and you observe it through your platform's native reactive primitive.
- **A write intent** — a durable publish obligation. NMP carries it through local acceptance, signing, routing, retry, and per-relay outcomes — and reports what it actually observed, not a misleading global-success boolean.

```text
YOUR APP  ── live queries / write intents ──▶  NMP  ──▶  Nostr relays & signers
 state · nav · identity · UI                 store · routing · outbox · diagnostics
```

## See it work

With [Rust](https://www.rust-lang.org/tools/install) installed:

```bash
git clone https://github.com/pablof7z/nmp.git
cd nmp
cargo run -p nmp-demo -- --secs 20
```

- Connects to two public indexers, **discovers author relays**, streams real events.
- Prints the relay plan and wire activity it observed.
- No Nostr key required. It's a running falsifier, not the shape of the public API.

## What you get today

Tags: ✅ solid & test-proven · 🧪 experimental / partial · ⛔ not yet

**Reading & state**
- ✅ Declarative live queries with native reactive bindings (`$currentPubkey`, derived projections, set algebra)
- ✅ Canonical **redb** store: provenance-preserving dedup, replaceable events, NIP-40 expiry (event-driven), kind:5 deletion + permanent tombstones
- ✅ Exact negative-delta supersession — stable handles update in place, no full re-query
- ✅ **Scoped acquisition evidence** — rows plus per-source facts; never a global "synced" / "complete"
- ✅ **Freshness axis on query demand** — `MaxAge`/`CacheOnly` served straight from per-handle coverage watermarks, no extra network round-trip to answer "is this fresh enough" ([#565](https://github.com/pablof7z/nmp/issues/565) closed, [#577](https://github.com/pablof7z/nmp/pull/577))
- ✅ **Bounded ordinary row delivery under a slow consumer** — the per-observer channel is a one-slot mailbox, not an unbounded queue: skipped reducer batches compose into a single exact transition rebased onto the last delivered state, so memory is bounded by the semantic gap between what an app last saw and current state, never by how many updates it missed ([#586](https://github.com/pablof7z/nmp/pull/586), progresses [#46](https://github.com/pablof7z/nmp/issues/46) — receipt observation, graph/derived-set ceilings, and scheduler limits stay open)
- ✅ **Windowing is a policy on the one read noun** — `observe(query, window)`; the parallel `History*` noun is gone. Delivery derives from boundedness (unbounded ⇒ deltas, windowed ⇒ authoritative snapshot), `AtBound` is a delivered fact not an error, and a deep scroll now holds **O(1)** live subscriptions per relay (closes [#474](https://github.com/pablof7z/nmp/issues/474)/[#485](https://github.com/pablof7z/nmp/issues/485)/[#486](https://github.com/pablof7z/nmp/issues/486))
- ✅ Per-handle freshness — `Live`, coverage-backed `MaxAge`, and strict no-wire `CacheOnly` compose with source/cache authority without changing query identity ([#565](https://github.com/pablof7z/nmp/issues/565), PR #577)

**Relays & networking**
- ✅ Full connection lifecycle behind **one finite fan-out ceiling** over the whole read plan
- ✅ Local / private / link-local / `.onion` targets **rejected by default** — resolved-IP admission is pinned per connection, closing a DNS-rebinding gap where a re-resolve could point an already-approved host somewhere internal
- ✅ Permanently-failing relays retire cleanly instead of wedging a connection slot; the send queue behind them is bounded
- ✅ **Self-bootstrapping NIP-65 outbox routing** — configure only indexers; the engine discovers each author's write/inbox relays
- ✅ Parse-once typed ingest with bounded parallel signature verification
- ✅ NIP-11 relay metadata (single-flight, LRU-bounded, proven raw-body ceiling)
- ✅ NIP-77 negentropy with a gap-free live handoff — a distinct `REQ {limit:0}` reaches EOSE first, remains open through reconciliation/backfill, and reconnect repeats the same order; deterministic boundary/timeout/error falsifiers plus a genuine NIP-77 relay prove the flow. A follow-up ([#579](https://github.com/pablof7z/nmp/pull/579)) closed a subscription leak in the live-EOSE-timeout fallback path, where an orphaned `limit:0` candidate REQ could linger and mint phantom coverage.
- ✅ **NIP-42 AUTH — content-relay authentication, complete end-to-end from Rust through Swift/Kotlin** ([#8](https://github.com/pablof7z/nmp/issues/8), closed). Six adversarially-reviewed waves landed it: Wave 1 keys relay identity/attribution/coverage/admission by **session, not URL** (`AccessContext { Public, Nip42(pubkey) }` + `RelaySessionKey`), passing an adversarial identity-isolation review clean ([#539](https://github.com/pablof7z/nmp/pull/539)). Wave 2 adds the **AUTH reducer + epoch state machine**: challenge epochs, a frozen `kind:22242` auth-event template (id commits to every field), AUTH-OK kept structurally disjoint from a durable write ACK, and authenticated write sessions — an eight-invariant adversarial review caught and fixed a real missed-wakeup, then re-verified clean ([#541](https://github.com/pablof7z/nmp/pull/541)). Wave 3 adds **runtime capability binding** (`AuthPolicy` trait, bounded registry, `Handle::{add,remove}_auth_policy`) and a **real-WebSocket AUTH capstone**: an in-repo strict relay proves `challenge → policy → sign → AUTH → OK → REQ → EOSE → rows` end-to-end, plus denial-parking, a fresh challenge on reconnect, and a wrong-challenge oracle — all 8 lifecycle/leak invariants passed adversarial review clean ([#542](https://github.com/pablof7z/nmp/pull/542)). Wave 5 projects that onto the **app-facing Rust facade**: a registrable `AuthPolicy` trait, `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)` (closes [#495](https://github.com/pablof7z/nmp/issues/495)), and per-session auth diagnostics — every type **facade-owned** rather than re-exported, so the governed 30,000-line facade ceiling actually **shrank** (29,957 → 29,557) instead of being raised ([#543](https://github.com/pablof7z/nmp/pull/543)). Wave 6 projects the whole surface to **FFI + Swift + Kotlin**: an `NMPAuthPolicy`/`FfiAuthPolicy` callback with a resolve/cancel completion object, `auth_sessions` diagnostics, and typed capability-exhaustion errors — a 7/7 adversarial race suite passed clean, facade snapshot untouched at 29,557/30,000 ([#544](https://github.com/pablof7z/nmp/pull/544)). Net result: a native iOS/Android/desktop app can register an `AuthPolicy`, resolve or deny a relay's challenge, do authenticated content-relay reads and writes, and read per-session auth diagnostics — proven against a real strict-AUTH relay with a non-vacuous wrong-challenge oracle. Honest remaining gaps: no standard Keychain/Keystore secure-signer providers yet (see Signing & identity below), and engine shutdown can still block on an app-owned pending-cancel hook that never returns — an app-hook contract issue, not specific to AUTH (see [known gaps](docs/known-gaps.md))

**Signing & identity**
- ✅ Local key signer — secret held in a `Zeroizing<[u8;32]>` (compiler-fenced wipe on drop), `Debug` redacted to public key only ([#47 Unit C](https://github.com/pablof7z/nmp/pull/546))
- ✅ Full **NIP-46 bunker** — independent signer-relay connection, request correlation, `auth_url`/`switch_relays`, NIP-44 crypto, **reconnect across store close/reopen**, bounded sign-only across all four surfaces (Rust/FFI/Swift/Kotlin)
- ✅ Per-write identity override — publish a single write under a registered secondary identity without changing the active account, across Rust/FFI/Swift/Kotlin. Retarget-immunity is proven: once accepted under the override, a later `set_active_account` can never redirect it to a different signer, even across a store close/reopen ([#47](https://github.com/pablof7z/nmp/issues/47) Unit A, [#550](https://github.com/pablof7z/nmp/pull/550))
- ✅ Platform secure-vault account stores — Keychain-backed (Swift, iOS/macOS) and JVM `KeyStore`-backed (Kotlin/desktop) checkpoint providers for automatic secure session restore ([#47](https://github.com/pablof7z/nmp/issues/47) vault providers, [#554](https://github.com/pablof7z/nmp/pull/554))
- ✅ Frozen identity on a parked write (`AwaitingCapability{pubkey}`) — a stranded reattached write now carries the exact pubkey it's still waiting on, not just "still parked." The PR's own cross-surface parity test caught direct-Rust and FFI reporting two *different* frozen pubkeys for the same receipt pre-merge, was fixed, and re-verified clean ([#47](https://github.com/pablof7z/nmp/issues/47) Unit B, [#556](https://github.com/pablof7z/nmp/pull/556))
- ✅ **#47 signer-lifecycle epic is fully closed** — all four units (zeroization, per-write override, reattachment, platform vaults) merged across Rust/FFI/Swift/Kotlin
- ⛔ No NIP-55 (Android intent-based signing)

**Publishing**
- ✅ **Durable write intents** — `Accepted` is one atomic persistence boundary (frozen body, receipt, pending row visible to queries)
- ✅ **Explicit pre-signature write cancellation** — `Engine::cancel(ReceiptId)` (Rust/FFI/Swift/Kotlin) atomically compensates the optimistic row, restores any displaced predecessor, persists a durable `Cancelled` receipt fact, and cancels in-flight signer work. Idempotent; a write that already signed returns a precise typed refusal instead of silently no-op'ing ([#533](https://github.com/pablof7z/nmp/issues/533) closed, [#585](https://github.com/pablof7z/nmp/pull/585))
- ✅ Signature promotion, internal-failure cancellation + compensation, persisted **bounded-retry outbox** (32 global / 1 per relay, deterministic backoff)
- ✅ At-most-once ambiguity becomes `OutcomeUnknown` — never a blind resend
- ✅ Verbatim publish of externally pre-signed events

**Protocol modules** (opt-in — core stays kind-agnostic)
- ✅ NIP-02 following — canonical kind:3, guarded tag-preserving follow/unfollow, on **Swift + Kotlin**
- ✅ NIP-29 groups — metadata / membership / moderation, plus kind:9 group-chat **send + read** proven by a live round-trip test (device-scale room-open UX still to be re-measured)
- ✅ Optional parser-only content module (source-ranged plaintext/Markdown and NIP-19 occurrences), pure safe reference-demand planning shared by Rust/Swift/Kotlin, and a SwiftUI family whose app-selected components—not parsing or visibility—own optional query handles. Exact kind:0/NIP-23 codecs belong to their own protocol owners, not `nmp-content` ([#561](https://github.com/pablof7z/nmp/issues/561))
- 🧪 NIP-51 lists — decode/reading only today; list **editing** is deliberately gated on [#50](https://github.com/pablof7z/nmp/issues/50)
- 🧪 Blossom (BUD-11) media/blob — `nmp-blossom` ships kind:24242-authorized, sha256-verified blob upload plus mirror/delete/list, each with its own bound authorization ([#216](https://github.com/pablof7z/nmp/issues/216) epic, closes [#545](https://github.com/pablof7z/nmp/issues/545)/[#551](https://github.com/pablof7z/nmp/issues/551), [#552](https://github.com/pablof7z/nmp/pull/552)/[#557](https://github.com/pablof7z/nmp/pull/557)) — and **projected through FFI to Swift and Kotlin** ([#555](https://github.com/pablof7z/nmp/issues/555) closes, [#560](https://github.com/pablof7z/nmp/pull/560) merged): a native app can call upload/mirror/delete/list from Rust, Swift, or Kotlin today, each with typed error taxonomies and no collapsed variants. Upload durability is currently **app-owned** (a standalone async call, not yet a persisted/retried engine obligation) — an engine-integrated durable-upload upgrade is tracked as an explicit additive follow-up ([#562](https://github.com/pablof7z/nmp/issues/562)), not a silent gap.
- ✅ NIP-68 picture events — `nmp-nip68` builds an unsigned kind:20 draft with `imeta` images minted only from a verified, content-addressed Blossom `BlobDescriptor`, plus a tolerant decoder that surfaces a missing sha256 as recorded diagnostics rather than trusting it ([#558](https://github.com/pablof7z/nmp/issues/558) closes, [#566](https://github.com/pablof7z/nmp/pull/566) merged). `build_picture` now takes an explicit `created_at` instead of sampling the clock — a determinism/FFI-parity fix ([#568](https://github.com/pablof7z/nmp/pull/568)). Engine-free, signing-free, first-cut tags only (`title`/`imeta`/`content-warning`/`t`); FFI/Swift/Kotlin projection is a separate later unit.
- ✅ Upload-then-publish composition — the new `nmp-media` crate wires `prepare → upload → compose` into three witness-typed stages so a skipped stage is unrepresentable: `prepare` holds the exact bytes it hashed/authorized (an authorized-hash/uploaded-bytes mismatch is structurally impossible), `PreparedUpload::upload` is a used-once obligation yielding a verified asset, and `compose_picture` hands the app an unsigned kind:20 for the *existing* `publish()` path. Upload failure, publish failure, and success are three **separate error types** (`PrepareError`/`MediaUploadError`/`MediaComposeError`), never one collapsed boolean — closes [#559](https://github.com/pablof7z/nmp/issues/559) (T15-C, [#575](https://github.com/pablof7z/nmp/pull/575) merged). The crate owns no event kind and exports no `claims()`; still not in this unit: durable upload ([#562](https://github.com/pablof7z/nmp/issues/562)), the FFI/Swift/Kotlin projection, and BUD-03 server-list placement.
- ⛔ No NIP-25 reactions, no general draft composition

**Storage**
- ✅ Crash-safe redb: binary canonical rows, secondary + tag + cardinality indexes, interned relay URLs
- ✅ In-memory store for tests
- ✅ Destructive reset that structurally **refuses to delete a live store**
- 🧪 Cross-process reset exclusion (no advisory/sidecar lock yet)

**Platforms**
- ✅ Rust core (the source of truth)
- 🧪 Swift SDK — qualified on the macOS host; XCFramework simulator slices compile, iOS-Simulator runtime target [pending](https://github.com/pablof7z/nmp/issues/465)
- 🧪 Kotlin SDK — desktop-JVM projection; **no Android AAR** qualified yet

## Status / maturity

- **Pre-1.0, pre-v2.** The v2 *semantic surface is freezing*; public names and shapes are provisional but governed.
- **Proven:** the core store, resolver, router, transport, engine, Rust facade, Swift + Kotlin packages, and the NIP-46 remote-signer path — backed by 100+ Rust test modules, differential falsifiers against an independent store, and live-relay tests.
- **Pending:** several promoted guarantees remain active work — see [`docs/known-gaps.md`](docs/known-gaps.md) (honest built-vs-missing record) and the [bug-class ledger](docs/bug-class-ledger.md) (target vs partial vs structurally proven).
- The ownership boundary and behavioral invariants are the stable frame; the app-facing spelling is not.
- **Headline (merged):** history is no longer a second noun — `observe(query, window)` makes windowing a policy on the one read noun, delivery mode derives from boundedness, and the #486 per-advance relay-REQ leak is fixed (deep scroll now holds O(1) live subscriptions per relay). Closes [#474](https://github.com/pablof7z/nmp/issues/474)/[#485](https://github.com/pablof7z/nmp/issues/485)/[#486](https://github.com/pablof7z/nmp/issues/486) — [#531](https://github.com/pablof7z/nmp/pull/531).
- **Recent hardening batch (merged):** a DNS-rebinding relay-admission gap closed, a permanently-failed-relay wedge + unbounded send queue fixed, three unbounded-memory bookkeeping structures pruned, Swift/Kotlin config parity gaps closed, wake-relay lane lookups indexed instead of full-scanned, `MemoryStore` secondary indexes + batched GC landed, and kind-ownership exclusivity now has real enforcement (it was previously documented but unenforced — [#521](https://github.com/pablof7z/nmp/issues/521)).
- **Proven this session:** end-to-end relay ingest holds up at real scale — a real websocket-to-redb harness pushed 1,000,000 signed events through the actual transport/verifier/resolver path with exact persistence on reopen ([#535](https://github.com/pablof7z/nmp/pull/535), closes [#530](https://github.com/pablof7z/nmp/issues/530)). It also found the next gap honestly, and that gap is now closed too: the same run's peak RSS fell 803,774,464 → 122,511,360 bytes (-84.76%, +13.57% throughput) after bounding redb's page cache to an explicit 64 MiB ceiling ([#540](https://github.com/pablof7z/nmp/pull/540), closes [#534](https://github.com/pablof7z/nmp/issues/534)).
- **Headline (merged) — NIP-42 is complete; issue #8 is closed.** Content-relay authentication landed across six adversarially-reviewed waves this arc. Wave 1, access-scoped session identity ([#539](https://github.com/pablof7z/nmp/pull/539)), keyed relay identity/attribution/coverage/admission by `(relay, access)` instead of URL, closing the structural cross-account-credit gap (bug-class ledger #18) *before* any AUTH negotiation exists — passed an adversarial identity-isolation review clean. Wave 2, the AUTH reducer + epoch state machine ([#541](https://github.com/pablof7z/nmp/pull/541)), adds challenge epochs, a frozen `kind:22242` auth-event template, AUTH-OK kept structurally disjoint from a write ACK, and authenticated write sessions — an eight-invariant adversarial review found and fixed one real missed-wakeup, then re-verified clean. Wave 3, runtime capability binding + the real-WebSocket AUTH capstone ([#542](https://github.com/pablof7z/nmp/pull/542)), proves it against a real challenging relay: `challenge → policy → sign → AUTH → OK → REQ → EOSE → rows`, denial-parking, a fresh challenge on reconnect, and a wrong-challenge oracle — all 8 lifecycle/leak invariants passed adversarial review, "no correctness holes." Wave 5 ([#543](https://github.com/pablof7z/nmp/pull/543)) projected that onto the supported `nmp` facade: a registrable `AuthPolicy` trait, `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)` (closes [#495](https://github.com/pablof7z/nmp/issues/495)), and per-session auth diagnostics — landed under the governed facade ceiling via facade-owned types, so the snapshot actually **shrank** 29,957 → 29,557/30,000 rather than raising the guardrail. Wave 6 ([#544](https://github.com/pablof7z/nmp/pull/544)) closed it out: the FFI + Swift + Kotlin projection — `NMPAuthPolicy`/`FfiAuthPolicy`, a resolve/cancel completion object, `auth_sessions` diagnostics, typed capability-exhaustion errors — passed a 7/7 adversarial race suite clean, facade snapshot untouched. Every wave was independently adversarially reviewed, and the whole epic stayed **under** the governed 30,000-line facade ceiling the entire time — the guardrail was never bypassed, only deferred or redesigned around. Frame it honestly: an iOS/Android/desktop app can now register an `AuthPolicy`, resolve/deny relay challenges, do authenticated content-relay reads/writes, and read per-session auth diagnostics — proven against a real strict-AUTH relay. Remaining, honestly: no standard Keychain/Keystore secure-signer providers yet, and an app-owned pending-cancel hook that never returns can still block engine shutdown (not AUTH-specific — see [known gaps](docs/known-gaps.md)); a macOS-only flake in the ingest-smoke suite ([#538](https://github.com/pablof7z/nmp/issues/538), closed via [#581](https://github.com/pablof7z/nmp/pull/581) — two real O_NONBLOCK-on-`accept()` races in the mock, not a product bug) is now fixed.
- **Superseded:** [`remove_account` (#529)](https://github.com/pablof7z/nmp/pull/529) was closed — its pubkey-only shape contradicted #8's ratified `AccountRegistration` model. Wave 5 replaced it with `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)`, which also closes [#495](https://github.com/pablof7z/nmp/issues/495).
- **Headline (merged) — architecture review is now enforced by CI, not just convention.** [#547](https://github.com/pablof7z/nmp/pull/547) closes [#496](https://github.com/pablof7z/nmp/issues/496): `AGENTS.md` gets a checked Noun / Reachability / Bool-Lifecycle / Destructive-API review-gate list (the exact discipline that caught `History*` but missed [#489](https://github.com/pablof7z/nmp/issues/489)), backed by two new blocking CI jobs — cross-SDK parity (Swift/Kotlin FFI surface must match Rust, modulo one documented exception) and falsifier-honesty (a PR's claimed `Updated falsifiers:` symbols/paths must actually exist in the tree). Backtested clean against 8 recent merged PRs / 43 named claims, and catches a fabricated claim plus a simulated #489-class regression.
- **Merged — signer hardening:** `LocalKeySigner`'s secret is now held in a `Zeroizing<[u8;32]>` with a redacted `Debug` impl ([#47](https://github.com/pablof7z/nmp/issues/47) Unit C, [#546](https://github.com/pablof7z/nmp/pull/546)) — the first landed unit of the broader signer-lifecycle epic.
- **Merged — #47 signer-lifecycle epic, Unit A:** per-write identity override across Rust/FFI/Swift/Kotlin ([#550](https://github.com/pablof7z/nmp/pull/550)) — publish under a registered secondary identity without moving `currentPubkey`; retarget-immunity is proven directly, including across a real redb close/reopen replay.
- **Merged — #47 signer-lifecycle epic, vault providers:** the secure-storage providers staged behind Unit A landed — a Keychain-backed account store (Swift, iOS/macOS) and a JVM `KeyStore`-backed account store (Kotlin/desktop), both restoring a session automatically ([#554](https://github.com/pablof7z/nmp/pull/554)).
- **Headline (merged) — #47 signer-lifecycle epic is complete; issue #47 is closed.** Unit B ([#556](https://github.com/pablof7z/nmp/pull/556)) carries the exact frozen pubkey on `WriteStatus::AwaitingCapability` so a parked write's stranded identity is observable, not just "still parked." Its own cross-surface parity suite caught direct-Rust and FFI reattach reporting two genuinely *different* frozen pubkeys for the same receipt pre-merge — the review net catching a real bug before it shipped — was fixed, and merged clean. Combined with per-write override (Unit A, [#550](https://github.com/pablof7z/nmp/pull/550)), platform vault providers ([#554](https://github.com/pablof7z/nmp/pull/554)), and the earlier zeroize-hardening (Unit C, [#546](https://github.com/pablof7z/nmp/pull/546)), all four units are now merged across Rust/FFI/Swift/Kotlin and #47 is closed.
- **Headline (merged) — Blossom (#216) T15-A is complete end-to-end.** [#560](https://github.com/pablof7z/nmp/pull/560) closes [#555](https://github.com/pablof7z/nmp/issues/555): `nmp-ffi` takes `nmp-blossom` as a direct dependency and projects upload/mirror/delete/list to Swift (`Blossom.swift`) and Kotlin (`Blossom.kt`), with per-operation error enums mirroring every Rust taxonomy variant 1:1. Cross-SDK parity, falsifier-honesty, surface-governance, `swift-package`, and `kotlin-package` CI all passed clean on merge — this was previously red on a real `[UInt8]`→`Data` mismatch, now fixed and verified, not just re-flaked. Combined with the merged core ([#552](https://github.com/pablof7z/nmp/pull/552)) and verbs ([#557](https://github.com/pablof7z/nmp/pull/557)), all three T15-A units are in: **Blossom media/blob is now callable from Rust, Swift, and Kotlin.** The owner also ruled on upload durability ([#559](https://github.com/pablof7z/nmp/issues/559) decision): ship standalone async upload now, with engine-integrated durable upload as an explicit additive upgrade later, not a non-goal — filed as [#562](https://github.com/pablof7z/nmp/issues/562).
- **Merged — NIP-68 picture events (T15-B):** `nmp-nip68` closes [#558](https://github.com/pablof7z/nmp/issues/558) ([#566](https://github.com/pablof7z/nmp/pull/566)) — builds a kind:20 draft with `imeta` images minted only from verified Blossom assets, plus a tolerant decoder. A same-day follow-up ([#568](https://github.com/pablof7z/nmp/pull/568)) threaded an explicit `created_at` through `build_picture` for determinism/FFI parity.
- **Headline (merged) — upload-then-publish composition (T15-C) is in; Blossom epic #216's core arc is complete.** [#575](https://github.com/pablof7z/nmp/pull/575) closes [#559](https://github.com/pablof7z/nmp/issues/559): a new `nmp-media` crate wires `prepare → upload → compose` as three witness-typed stages over the *existing* `publish()` path, with upload failure, publish failure, and success kept as three genuinely separate error types rather than one collapsed boolean. Combined with T15-A (Blossom core/verbs/SDK, [#552](https://github.com/pablof7z/nmp/pull/552)/[#557](https://github.com/pablof7z/nmp/pull/557)/[#560](https://github.com/pablof7z/nmp/pull/560)) and T15-B (NIP-68 picture schema, [#566](https://github.com/pablof7z/nmp/pull/566)/[#568](https://github.com/pablof7z/nmp/pull/568)), the full upload→picture-event→publish arc is proven end-to-end in Rust. Epic [#216](https://github.com/pablof7z/nmp/issues/216) stays open, honestly, for what's left: the batched FFI/Swift/Kotlin projection of NIP-68 + `nmp-media` (Rust-only today), durable upload as an additive upgrade ([#562](https://github.com/pablof7z/nmp/issues/562)), and BUD-03 server-list placement.
- **Fixed — NIP-77 subscription leak (follow-up to #570):** [#579](https://github.com/pablof7z/nmp/pull/579) closes an orphaned live-candidate leak in the negentropy live-EOSE-timeout fallback path, where a `limit:0` candidate REQ could linger unclosed and mint phantom coverage.
- **Merged — freshness axis on query demand:** [#577](https://github.com/pablof7z/nmp/pull/577) closes [#565](https://github.com/pablof7z/nmp/issues/565) — `MaxAge`/`CacheOnly` are now served directly from per-handle coverage watermarks.
- **Headline (merged) — explicit pre-signature write cancellation:** [#585](https://github.com/pablof7z/nmp/pull/585) closes [#533](https://github.com/pablof7z/nmp/issues/533) — an accepted-but-unsigned write used to be able to sit indefinitely with no receipt-keyed way to retract it. Now `cancel(receiptId)` is a durable, typed, idempotent operation across Rust/FFI/Swift/Kotlin: success atomically compensates the optimistic row, restores any displaced predecessor, persists a `Cancelled` receipt fact in the same transaction, and releases in-flight signer ownership; a write that already crossed the signature boundary returns a precise typed refusal, never a silent no-op. Adversarial review caught and drove fixes for signer-task leaks, quarantined recovered writes, and signed-ephemeral replay before merge.
- **Merged — bounded ordinary row delivery under a slow consumer:** [#586](https://github.com/pablof7z/nmp/pull/586) replaces the per-observer unbounded `mpsc` channel with a one-slot mailbox — skipped reducer batches compose per event-id into one exact transition rebased onto the last delivered state, so a slow query consumer can no longer make the engine's memory grow or replay stale intermediate frames. Windowed rows/diagnostics already used one-slot latest snapshots; this closes the same gap for unwindowed ordinary delivery. Progresses [#46](https://github.com/pablof7z/nmp/issues/46) — receipt observation, graph/derived-set ceilings, relay-advertised limits, and scheduler/resource bounds stay open.
- **New epic underway, not yet merged — [#561](https://github.com/pablof7z/nmp/issues/561):** separating content *parsing* from component-owned Nostr reference *acquisition*. Three drafts are open against it ([#569](https://github.com/pablof7z/nmp/pull/569), [#578](https://github.com/pablof7z/nmp/pull/578), [#582](https://github.com/pablof7z/nmp/pull/582)) reassigning Swift/Kotlin reference-acquisition ownership — none merged yet.
- **Also open:** a consolidated **v2 architecture decision record** ([#548](https://github.com/pablof7z/nmp/issues/548), 15 rulings against standing doctrine) — now published as a browsable page with a spoken overall briefing plus a per-issue deep-dive: [pablof7z.github.io/nmp/v2-escalation](https://pablof7z.github.io/nmp/v2-escalation/).

## Performance

Built for **bounded memory and streaming — never first-N truncation.** Measured on a real ~1,100-event corpus / million-row fixture:

- Busiest-room query: **5.15 ms → 0.26 ms**
- Derived-set resolver over a **59,915-row** bucket: **3,786 ms → 0.73 ms**
- Rejected-heavy search: **0.188 ms → 0.005 ms**
- Router coalesce fixed-point: **O(n³) → O(n²)**, plan-identical output
- Ordinary query delivery to a slow observer is now bounded by a one-slot rebased mailbox instead of an unbounded per-update queue — memory tracks the semantic delta since last delivery, not the number of missed updates ([#586](https://github.com/pablof7z/nmp/pull/586))
- Query planning picks one best index and **stops at the visible limit** — no full-history materialization
- **Relay ingest proven end-to-end at real scale** — 1,000,000 signed events over the actual websocket/transport/verifier/resolver/redb path, all frames accounted for and exactly recovered on reopen: ~4,333 events/s, 4.96s p95 apply latency, 2.08 GB store ([#535](https://github.com/pablof7z/nmp/pull/535), closes [#530](https://github.com/pablof7z/nmp/issues/530)). Peak RSS during that same run is now bounded too — an explicit 64 MiB redb page-cache ceiling cut it 803,774,464 → 122,511,360 bytes (-84.76%), with +13.57% throughput ([#540](https://github.com/pablof7z/nmp/pull/540), closes [#534](https://github.com/pablof7z/nmp/issues/534))
- NIP-11 cache carries a **proven ~67 MiB raw-body ceiling** (not a total-RSS claim)
- Public Rust facade governed under a **30,000-line surface ceiling**, enforced by a trusted-base CI gate

## Platforms in one line

Rust core is the truth · **Swift** qualified on macOS host (iOS-sim runtime pending) · **Kotlin** desktop-JVM only (no Android AAR yet).

## Roadmap / where it's heading

- Govern the provisional demand / receipt / signer shapes toward a **v2 freeze**
- Encode lifecycle invariants **as types**, not conventions
- Close **platform qualification** — an iOS-Simulator test target, an Android AAR
- Finish **bounded delivery** with an explicit shortfall contract everywhere
- Land NIP-51 list editing; broaden opt-in protocol modules
- Project NIP-68 + `nmp-media` composition through FFI/Swift/Kotlin, batched together (currently Rust-only)
- Revisit engine-integrated durable upload as an additive upgrade over standalone async upload ([#562](https://github.com/pablof7z/nmp/issues/562)), now that T15-C composition has landed
- Separate content parsing from component-owned Nostr reference acquisition ([#561](https://github.com/pablof7z/nmp/issues/561) epic — drafts open, not merged: [#569](https://github.com/pablof7z/nmp/pull/569)/[#578](https://github.com/pablof7z/nmp/pull/578)/[#582](https://github.com/pablof7z/nmp/pull/582))
- Land NIP-51 relay-list editing ([#488](https://github.com/pablof7z/nmp/issues/488))
- **Shipped:** NIP-42 content-relay AUTH is complete end-to-end, Rust through Swift/Kotlin — all six waves merged, [#8](https://github.com/pablof7z/nmp/issues/8) closed. See Status / maturity above.
- **Shipped:** architecture-review discipline is now machine-enforced — cross-SDK parity and falsifier-honesty run as blocking CI checks ([#547](https://github.com/pablof7z/nmp/pull/547), closes [#496](https://github.com/pablof7z/nmp/issues/496)).
- **Shipped:** the **#47 signer-lifecycle epic is complete and closed** — zeroize-hardening, per-write identity override, reattachment with frozen-identity visibility, and Keychain/JVM-KeyStore vault providers all merged across Rust/FFI/Swift/Kotlin ([#546](https://github.com/pablof7z/nmp/pull/546)/[#550](https://github.com/pablof7z/nmp/pull/550)/[#556](https://github.com/pablof7z/nmp/pull/556)/[#554](https://github.com/pablof7z/nmp/pull/554)).
- **Shipped:** Blossom epic (#216) core arc — T15-A upload/mirror/delete/list + SDK projection ([#552](https://github.com/pablof7z/nmp/pull/552)/[#557](https://github.com/pablof7z/nmp/pull/557)/[#560](https://github.com/pablof7z/nmp/pull/560)), T15-B NIP-68 picture schema ([#566](https://github.com/pablof7z/nmp/pull/566)/[#568](https://github.com/pablof7z/nmp/pull/568)), and T15-C upload-then-publish composition ([#575](https://github.com/pablof7z/nmp/pull/575)) — callable end-to-end from Rust today; see Protocol modules above.
- **Shipped:** freshness axis on query demand — `MaxAge`/`CacheOnly` from coverage watermarks ([#577](https://github.com/pablof7z/nmp/pull/577), closes [#565](https://github.com/pablof7z/nmp/issues/565)).
- **Shipped:** explicit pre-signature write cancellation — receipt-keyed `cancel(receiptId)` across Rust/FFI/Swift/Kotlin, durable and idempotent ([#585](https://github.com/pablof7z/nmp/pull/585), closes [#533](https://github.com/pablof7z/nmp/issues/533)).
- **Shipped:** bounded ordinary row delivery under a slow consumer — one-slot rebased mailbox replaces the unbounded per-observer queue ([#586](https://github.com/pablof7z/nmp/pull/586), progresses [#46](https://github.com/pablof7z/nmp/issues/46)).

## The ownership boundary

| NMP owns | Your app owns | The UI framework owns |
|---|---|---|
| Canonical event & write-obligation storage | App state and architecture | Rendering and layout |
| Relay discovery, routing, sync, subscription lifecycle | Which queries and writes exist | Observation scope |
| Dedup, provenance, replacement, deletion, expiry | Account and identity experience | Navigation and presentation |
| Durable publication work and per-relay evidence | Ordering, moderation, product policy | Platform presentation details |
| Permanent diagnostics over all of the above | How evidence is explained to a person | — |

Diagnostics are a **permanent, read-only proof surface** — source plan, wire filters, connections, relay evidence, limits, write attempts — not a debug mode that changes behavior.

## Repo layout

- `crates/nmp` — the supported Rust facade (`nmp::Engine`); `crates/nmp-ffi` projects it to Swift/Kotlin via UniFFI
- `crates/nmp-{store,resolver,router,transport,engine,signer,executor}` — internal seams, not alternate APIs
- `crates/nmp-content` — optional parser-only semantic document layer
- `crates/nmp-{nip02,nip29,nip51,blossom,nip68,media}` — opt-in protocol modules
- `crates/nmp-demo` — the read-only CLI falsifier
- `Packages/NMP` (Swift) · `Packages/NMPKotlin` (Kotlin/JVM)
- `apps/Falsifier`, `apps/UIGallery` — SwiftUI proving grounds
- `docs/` — vision, design record, known gaps, surface baselines

## Start here

- [Builder guide](docs/builder/README.md) — product model, examples, platform guidance
- [Vision](docs/VISION.md) — north star and settled invariants
- [Design record](docs/design-record.md) — the exploration and decisions
- [Known gaps](docs/known-gaps.md) — the honest built-vs-missing list
- [Contributor guide](AGENTS.md) — issue-first workflow and verification discipline

## Security & trust boundary

- NMP runs **in the host app** and owns local cache + write-obligation state.
- The app owns identity import, backup, removal, and user-facing trust policy.
- An **explicitly insecure** plaintext file checkpoint exists for personal/dev autologin — opt-in, separate from the canonical store, and **not** a substitute for secure providers.
- Key-handling and secure-signer production readiness is tracked openly in [known gaps](docs/known-gaps.md).

## Contributing

Every unit of work starts with a GitHub issue that captures why it matters. Read [`AGENTS.md`](AGENTS.md), then pick from the [open issues](https://github.com/pablof7z/nmp/issues).

## License

[MIT](LICENSE)
