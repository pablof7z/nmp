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

**Relays & networking**
- ✅ Full connection lifecycle behind **one finite fan-out ceiling** over the whole read plan
- ✅ Local / private / link-local / `.onion` targets **rejected by default** — resolved-IP admission is pinned per connection, closing a DNS-rebinding gap where a re-resolve could point an already-approved host somewhere internal
- ✅ Permanently-failing relays retire cleanly instead of wedging a connection slot; the send queue behind them is bounded
- ✅ **Self-bootstrapping NIP-65 outbox routing** — configure only indexers; the engine discovers each author's write/inbox relays
- ✅ Parse-once typed ingest with bounded parallel signature verification
- ✅ NIP-11 relay metadata (single-flight, LRU-bounded, proven raw-body ceiling)
- ✅ NIP-77 negentropy — end-to-end set reconciliation, proven by a live falsifier against a real negentropy-speaking relay (reconnect temporarily replays as plain REQ — perf, not correctness)
- 🧪 NIP-42 AUTH — real and tested inside the NIP-46 signer connection; on **content** relays the participation gate + write-side `AwaitingAuth` exist but no challenge is answered yet and read-evidence states are reserved for [#8](https://github.com/pablof7z/nmp/issues/8)

**Signing & identity**
- ✅ Local key signer
- ✅ Full **NIP-46 bunker** — independent signer-relay connection, request correlation, `auth_url`/`switch_relays`, NIP-44 crypto, **reconnect across store close/reopen**, bounded sign-only across all four surfaces (Rust/FFI/Swift/Kotlin)
- ⛔ No Keychain/Keystore secure providers, no NIP-55, no per-write identity override, no secret zeroization yet

**Publishing**
- ✅ **Durable write intents** — `Accepted` is one atomic persistence boundary (frozen body, receipt, pending row visible to queries)
- ✅ Signature promotion, cancellation + compensation, persisted **bounded-retry outbox** (32 global / 1 per relay, deterministic backoff)
- ✅ At-most-once ambiguity becomes `OutcomeUnknown` — never a blind resend
- ✅ Verbatim publish of externally pre-signed events

**Protocol modules** (opt-in — core stays kind-agnostic)
- ✅ NIP-02 following — canonical kind:3, guarded tag-preserving follow/unfollow, on **Swift + Kotlin**
- ✅ NIP-29 groups — metadata / membership / moderation, plus kind:9 group-chat **send + read** proven by a live round-trip test (device-scale room-open UX still to be re-measured)
- ✅ Optional content module (plaintext/Markdown, NIP-19 refs, kind:0 / NIP-23) + a SwiftUI component family
- 🧪 NIP-51 lists — decode/reading only today; list **editing** is deliberately gated on [#50](https://github.com/pablof7z/nmp/issues/50)
- ⛔ No NIP-25 reactions, no general draft composition, no media/Blossom yet

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
- **Recent hardening batch (merged):** a DNS-rebinding relay-admission gap closed, a permanently-failed-relay wedge + unbounded send queue fixed, three unbounded-memory bookkeeping structures pruned, Swift/Kotlin cross-SDK parity gaps (config fields, content-session pause) closed, wake-relay lane lookups indexed instead of full-scanned, `MemoryStore` secondary indexes + batched GC landed, and kind-ownership exclusivity now has real enforcement (it was previously documented but unenforced — [#521](https://github.com/pablof7z/nmp/issues/521)).
- **In CI now (not yet merged):** `remove_account` across all four surfaces (Rust/FFI/Swift/Kotlin).

## Performance

Built for **bounded memory and streaming — never first-N truncation.** Measured on a real ~1,100-event corpus / million-row fixture:

- Busiest-room query: **5.15 ms → 0.26 ms**
- Derived-set resolver over a **59,915-row** bucket: **3,786 ms → 0.73 ms**
- Rejected-heavy search: **0.188 ms → 0.005 ms**
- Router coalesce fixed-point: **O(n³) → O(n²)**, plan-identical output
- Query planning picks one best index and **stops at the visible limit** — no full-history materialization
- NIP-11 cache carries a **proven ~67 MiB raw-body ceiling** (not a total-RSS claim)
- Public Rust facade governed under a **30,000-line surface ceiling**, enforced by a trusted-base CI gate

## Platforms in one line

Rust core is the truth · **Swift** qualified on macOS host (iOS-sim runtime pending) · **Kotlin** desktop-JVM only (no Android AAR yet).

## Roadmap / where it's heading

- Govern the provisional demand / receipt / signer shapes toward a **v2 freeze**
- Encode lifecycle invariants **as types**, not conventions
- Close **platform qualification** — an iOS-Simulator test target, an Android AAR
- Ship standard **secure-storage signer providers** (Keychain / Keystore)
- Finish **bounded delivery** with an explicit shortfall contract everywhere
- Populate NIP-42 AUTH evidence states; land NIP-51 list editing; broaden opt-in protocol modules
- **In progress:** fold paginated history into the existing read noun as `observe(query, window)` — a window policy, not a second parallel `History*` noun — retiring the earlier `HistoryQuery`/`HistoryContinuation` design

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
- `crates/nmp-{nip02,nip29,nip51,content}` — opt-in protocol modules
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
