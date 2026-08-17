# NMP

**An embeddable Nostr client engine. You bring the app; NMP owns the network.**

A Rust core with Swift and Kotlin SDKs that packages the hard Nostr client machinery — relay routing, outbox discovery, canonical state, signing, durable publishing — behind a small API you *call*. Not a framework you live inside.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

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
 state · nav · identity · UI                 store · routing · delivery · diagnostics
```

## Build & test

With [Rust](https://www.rust-lang.org/tools/install) installed:

```bash
git clone https://github.com/pablof7z/nmp.git
cd nmp
cargo test -p nmp --release
```

- Runs the in-repo Rust test suite for the supported facade and its
  internal seams.

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

**Relays & networking**
- ✅ Full connection lifecycle behind **one finite fan-out ceiling** over the whole read plan
- ✅ Valid relay targets use ordinary platform resolution and connection semantics, including loopback, private, link-local, and `.onion` hosts; reachability is an observed connection outcome, not a configuration grant
- ✅ Permanently-failing relays retire cleanly instead of wedging a connection slot; the send queue behind them is bounded
- ✅ **Author-route discovery is an adapter seam** — the engine declares
  neutral author-route needs and applies neutral facts; WHICH algorithm answers
  them is `AuthorRouteProvider`, an application-supplied implementation passed
  to `Engine::new_with_capabilities_and_routing`. `nmp-outbox` is the NIP-65
  outbox model (exact source queries, canonical kind:10002 winners, marker
  parsing, settlement, atomic fact replacement); a competing algorithm is a
  third-party crate depending on `nmp-engine`/`nmp-router`/`nmp-grammar` and
  touching no NMP source at all. One provider per engine, chosen at
  construction and fixed for its life — no registration, no swapping
- ✅ Parse-once typed ingest with bounded parallel signature verification
- ✅ NIP-11 relay metadata (single-flight, LRU-bounded, proven raw-body ceiling)
- ✅ NIP-77 negentropy with a gap-free live handoff — a distinct `REQ {limit:0}` reaches EOSE first, remains open through reconciliation/backfill, and reconnect repeats the same order; deterministic boundary/timeout/error falsifiers plus a genuine NIP-77 relay prove the flow. A follow-up ([#579](https://github.com/pablof7z/nmp/pull/579)) closed a subscription leak in the live-EOSE-timeout fallback path, where an orphaned `limit:0` candidate REQ could linger and mint phantom coverage.
- ⚠️ **NIP-42 AUTH — content-relay authentication, wired end-to-end from Rust through Swift/Kotlin; authenticated writes work, protected reads deadlock against most deployed relays** ([#8](https://github.com/pablof7z/nmp/issues/8), closed; [#1889](https://github.com/pablof7z/nmp/issues/1889), open). Six adversarially-reviewed waves landed it: Wave 1 keys relay identity/attribution/coverage/admission by **session, not URL** (`AccessContext { Public, Nip42(pubkey) }` + `RelaySessionKey`), passing an adversarial identity-isolation review clean ([#539](https://github.com/pablof7z/nmp/pull/539)). Wave 2 adds the **AUTH reducer + epoch state machine**: challenge epochs, a frozen `kind:22242` auth-event template (id commits to every field), AUTH-OK kept structurally disjoint from a durable write ACK, and authenticated write sessions — an eight-invariant adversarial review caught and fixed a real missed-wakeup, then re-verified clean ([#541](https://github.com/pablof7z/nmp/pull/541)). Wave 3 adds **runtime capability binding** (`AuthPolicy` trait, bounded registry, `Handle::{add,remove}_auth_policy`) and a **real-WebSocket AUTH capstone**: an in-repo strict relay proves `challenge → policy → sign → AUTH → OK → REQ → EOSE → rows` end-to-end, plus denial-parking, a fresh challenge on reconnect, and a wrong-challenge oracle — all 8 lifecycle/leak invariants passed adversarial review clean ([#542](https://github.com/pablof7z/nmp/pull/542)). Wave 5 projects that onto the **app-facing Rust facade**: a registrable `AuthPolicy` trait, `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)` (closes [#495](https://github.com/pablof7z/nmp/issues/495)), and per-session auth diagnostics — the snapshot records **facade-owned** rather than re-exported ([#543](https://github.com/pablof7z/nmp/pull/543)), though the closed AUTH phase vocabulary itself is one engine-owned type re-exported to every surface ([#1616](https://github.com/pablof7z/nmp/issues/1616)). Wave 6 projects the whole API to **FFI + Swift + Kotlin**: an `NMPAuthPolicy`/`FfiAuthPolicy` callback with a resolve/cancel completion object, `auth_sessions` diagnostics, and typed capability-exhaustion errors — a 7/7 adversarial race suite passed clean ([#544](https://github.com/pablof7z/nmp/pull/544)). Net result: a native iOS/Android/desktop app can register an `AuthPolicy`, resolve or deny a relay's challenge, do authenticated content-relay writes, and read per-session auth diagnostics — proven against a real strict-AUTH relay with a non-vacuous wrong-challenge oracle. Honest remaining gaps: **protected reads deadlock against any relay that challenges in response to a request rather than unsolicited on connect ([#1889](https://github.com/pablof7z/nmp/issues/1889)) — which includes strfry, and so most deployed relays; the query never transmits a byte and the installed `AuthPolicy` is never consulted**; no standard Keychain/Keystore secure-signer providers yet (see Signing & identity below); and engine shutdown can still block on an app-owned pending-cancel hook that never returns — an app-hook contract issue, not specific to AUTH (see [known gaps](docs/known-gaps.md))

**Signing & identity**
- ✅ Local key signer — one fixed-allocation, non-`Clone` canonical zeroizing secret owner (moving the signer relocates only its pointer), with operation-scoped BIP-340/NIP-44 secret, key, hash-state, cipher-state, and plaintext owners that wipe on success, refusal, and unwind; no operational `nostr::Keys`/`SecretKey`/`Keypair` is retained, and `Debug` is redacted to the public key only ([#546](https://github.com/pablof7z/nmp/pull/546) began this; [#765](https://github.com/pablof7z/nmp/issues/765) replaced its unused duplicate with the real operational owner)
- ✅ Per-write identity override — publish a single write under a secondary session account without changing the current account, across Rust/FFI/Swift/Kotlin. Retarget-immunity is proven: once accepted under the override, a later account switch can never redirect it to a different signer, even across a store close/reopen ([#47](https://github.com/pablof7z/nmp/issues/47) Unit A, [#550](https://github.com/pablof7z/nmp/pull/550))
- ✅ Whole-session account model — signer-backed and public-key-only accounts, optional current selection, and provider reconstruction material export and restore as one opaque value. Provider reachability is runtime state, never a reason to drop the account from the restored session ([#1397](https://github.com/pablof7z/nmp/issues/1397))
- ✅ Frozen identity on a parked write (`AwaitingCapability{pubkey}`) — a stranded reattached write now carries the exact pubkey it's still waiting on, not just "still parked." The PR's own cross-API parity test caught direct-Rust and FFI reporting two *different* frozen pubkeys for the same receipt pre-merge, was fixed, and re-verified clean ([#47](https://github.com/pablof7z/nmp/issues/47) Unit B, [#556](https://github.com/pablof7z/nmp/pull/556))
- ✅ Frozen write identity and local-key zeroization are projected across Rust/FFI/Swift/Kotlin; app-owned transactional session storage remains tracked separately in [#1398](https://github.com/pablof7z/nmp/issues/1398)
- ⛔ No NIP-55 (Android intent-based signing)

**Publishing**
- ✅ **Durable write intents** — `Accepted` is one atomic persistence boundary (frozen body, receipt, pending row visible to queries)
- ✅ **Replaceable delivery coalescing and disposal** — a newer kind `0`, `3`, `10000...19999`, or same-`d` `30000...39999` write destroys the older event body, route, lanes, attempts, and deadlines instead of replaying obsolete bytes. Work proved never handed off leaves no receipt; possible-handoff ambiguity keeps only a typed `Superseded` safety receipt in the same internally bounded terminal history as every other completed write. Already-expired writes are refused before custody and retain nothing.
- ✅ **Explicit pre-signature write cancellation** — `Engine::cancel(ReceiptId)` (Rust/FFI/Swift/Kotlin) atomically compensates the optimistic row, restores a relay-observed displaced predecessor when one exists, never resurrects obsolete unpublished local history, persists a durable `Cancelled` receipt fact, and cancels in-flight signer work. Idempotent; a write that already signed returns a precise typed refusal instead of silently no-op'ing ([#533](https://github.com/pablof7z/nmp/issues/533) closed, [#585](https://github.com/pablof7z/nmp/pull/585))
- ✅ Signature promotion, internal-failure cancellation + compensation, persisted **bounded-retry delivery** (32 global / 1 per relay, deterministic backoff)
- ✅ At-most-once ambiguity becomes `OutcomeUnknown` — never a blind resend
- ✅ Verbatim publish of externally pre-signed events

**Protocol modules** (opt-in — core stays kind-agnostic)
- ✅ NIP-02 following — durable tag-preserving follow/unfollow over cached,
  first-value, and later relay source truth, with one ordinary receipt on
  **Swift + Kotlin**
- ✅ NIP-65 Rust module — `nmp-nip65` holds engine-free kind:10002 values
  (validation, composition, canonical winners, marker parsing); `nmp-outbox`
  turns them into an installable `AuthorRouteProvider`. Swift and Kotlin apps
  add the outbox-routing capability through the same committed `.nmp.toml` as
  every other native family, then configure `OutboxRoutingConfig(indexers:)` at
  engine runtime. Prepared cold-product capstones prove the configured indexer
  discovers the outbox, the write reaches only that learned relay, and no
  undeclared fixture relay is contacted.
- ✅ NIP-73 external content ids — the `(i, k)` pair naming something that is
  not a Nostr event, in its own crate because several NIPs consume them and
  none owns them. Podcast episodes, `web` URLs (canonicalised: normalised, no
  fragment), and an already-canonical general pair.
- ✅ NIP-22 comments over NIP-73 external content ids — typed root/parent
  validation, thread demand, decode, and deterministic composition across
  Rust, FFI, Swift, and Kotlin. Composition is an engine-free protocol
  function returning the ordinary `WriteIntent`; apps publish it through the
  one generic `publish` → `Receipt` lifecycle.
- ✅ Optional parser-only content module (source-ranged plaintext/Markdown and
  NIP-19 occurrences), exact five-variant locator values shared by
  Rust/Swift/Kotlin, and a SwiftUI family whose app-selected components—not
  parsing or visibility—ask an explicit app resolver for one ordinary demand.
  Core decoding owns no kind:0, source-authority, relay-admission, or hidden
  fan-out policy; exact kind:0/NIP-23 codecs belong to their own optional
  protocol owners ([#561](https://github.com/pablof7z/nmp/issues/561), corrected
  by [#879](https://github.com/pablof7z/nmp/issues/879))
- 🧪 NIP-29 groups — a group can live on more than one relay, so Rust, FFI,
  and the hand-written Swift/Kotlin SDKs expose the same relay-scope shape:
  `nmp::nip29::on(hosts)` returns a `RelayScope` (fallible — an app-supplied
  relay set can be empty), narrowed to one `Group` via `.group(id)`
  ([#1033](https://github.com/pablof7z/nmp/issues/1033);
  superseded the single-host `Group::new(host, id)` door with no alias). Every
  group write mints the ordinary `WriteIntent` and routes `Explicit` to the
  whole scope; every group read is one ordinary `LiveQuery`
  (`Single`/`Union` of per-host branches). Discovery is the ordinary query language —
  `nip29::groups_whose_record_matches(Filter)` names groups by any live-query
  filter over a relay-signed record, with
  `member_list_includes`/`admin_list_includes` as shorthands exactly equal to
  it and `any_of(Binding)` taking a derived id source, never claiming exact
  membership/admin state; `nip29::all()` is "every group this relay
  advertises", expressed as the absence of a `#d` constraint
  ([#1252](https://github.com/pablof7z/nmp/issues/1252)). The former kind:9 composer/content catalog remains
  removed because C7 owns chat and `q` replies. A direct-Rust and macOS-host
  Swift consumer exercised the public API against two real local relays;
  that evidence does not claim iOS-device or Android-runtime qualification.
- 🧪 NIP-29 remembered-groups product capability — `nmp-nip29` exposes observational reading of NIP-51 kind:10009, while `nmp::nip29` owns typed group and relay-in-use add/remove operations through the ordinary durable semantic-write receipt across Rust, FFI, Swift, and Kotlin ([#1552](https://github.com/pablof7z/nmp/issues/1552))
- 🧪 Blossom (BUD-11) media/blob — `nmp-blossom` ships kind:24242-authorized, sha256-verified blob upload plus mirror/delete/list, each with its own bound authorization ([#216](https://github.com/pablof7z/nmp/issues/216) epic, closes [#545](https://github.com/pablof7z/nmp/issues/545)/[#551](https://github.com/pablof7z/nmp/issues/551), [#552](https://github.com/pablof7z/nmp/pull/552)/[#557](https://github.com/pablof7z/nmp/pull/557)) — and **projected through FFI to Swift and Kotlin** ([#555](https://github.com/pablof7z/nmp/issues/555) closes, [#560](https://github.com/pablof7z/nmp/pull/560) merged): a native app can call upload/mirror/delete/list from Rust, Swift, or Kotlin today, each with typed error taxonomies and no collapsed variants. Upload durability is currently **app-owned** (a standalone async call, not yet a persisted/retried engine obligation) — an engine-integrated durable-upload upgrade is tracked as an explicit additive follow-up ([#562](https://github.com/pablof7z/nmp/issues/562)), not a silent gap.
- ✅ NIP-68 picture events — `nmp-nip68` builds an unsigned kind:20 draft with `imeta` images minted only from a verified, content-addressed Blossom `BlobDescriptor`, plus a tolerant decoder that surfaces a missing sha256 as recorded diagnostics rather than trusting it ([#558](https://github.com/pablof7z/nmp/issues/558) closes, [#566](https://github.com/pablof7z/nmp/pull/566) merged). `build_picture` now takes an explicit `created_at` instead of sampling the clock — a determinism/FFI-parity fix ([#568](https://github.com/pablof7z/nmp/pull/568)). Engine-free, signing-free, first-cut tags only (`title`/`imeta`/`content-warning`/`t`); FFI/Swift/Kotlin projection is a separate later unit.
- ✅ Upload-then-publish composition — the new `nmp-media` crate wires `prepare → upload → compose` into three witness-typed stages so a skipped stage is unrepresentable: `prepare` holds the exact bytes it hashed/authorized (an authorized-hash/uploaded-bytes mismatch is structurally impossible), `PreparedUpload::upload` is a used-once obligation yielding a verified asset, and `compose_picture` hands the app an unsigned kind:20 whose public body fields copy into `EventBuilder` for the *existing* `publish()` path (with its author selected explicitly). Upload failure, publish failure, and success are three **separate error types** (`PrepareError`/`MediaUploadError`/`MediaComposeError`), never one collapsed boolean — closes [#559](https://github.com/pablof7z/nmp/issues/559) (T15-C, [#575](https://github.com/pablof7z/nmp/pull/575) merged). The crate owns no event kind and exports no `claims()`; still not in this unit: durable upload ([#562](https://github.com/pablof7z/nmp/issues/562)), the FFI/Swift/Kotlin projection, and BUD-03 server-list placement.
- ⛔ No NIP-25 reactions, no general draft composition

**Storage**
- ✅ Crash-safe redb: binary canonical rows, secondary + tag + cardinality indexes, interned relay URLs
- ✅ Isolated temporary Redb stores for tests
- ✅ Destructive reset that structurally **refuses to delete a live store**
- 🧪 Cross-process reset exclusion (no advisory/sidecar lock yet)

**Platforms**
- ✅ Rust core (the source of truth)
- 🧪 Swift SDK — apps prepare one Cargo-resolved, feature-selected local package; public-wrapper behavior is qualified by macOS-host XCTest. iOS runtime and physical-device qualification remain separate.
- 🧪 Kotlin SDK — apps prepare the same feature-selected desktop-JVM module or
  Android AAR. The AAR has exact API-26 `arm64-v8a`/`x86_64` packaging and
  clean-consumer qualification. Its public facade is exercised by an external
  app on a pinned API-35 emulator, including controlled live/offline recovery,
  app-private restart, cancellation, close, wrong-ABI refusal, and bounded
  64-collector performance. Configuration lifecycle proof remains #833.

## Status / maturity

- **Pre-1.0, pre-v2.** The v2 *public API is freezing*; public names and shapes are provisional.
- **Proven:** the core store, resolver, router, transport, engine, Rust facade, and the Swift + Kotlin packages — backed by 100+ Rust test modules, real Redb semantic and crash-reopen falsifiers, and live-relay tests.
- **Pending:** several promoted guarantees remain active work — see [`docs/known-gaps.md`](docs/known-gaps.md) (honest built-vs-missing record) and the [bug-class ledger](docs/bug-class-ledger.md) (target vs partial vs structurally proven).
- The ownership boundary and behavioral invariants are the stable frame; the app-facing spelling is not.
- **Headline (merged):** history is no longer a second noun — `observe(query, window)` makes windowing a policy on the one read noun, delivery mode derives from boundedness, and the #486 per-advance relay-REQ leak is fixed (deep scroll now holds O(1) live subscriptions per relay). Closes [#474](https://github.com/pablof7z/nmp/issues/474)/[#485](https://github.com/pablof7z/nmp/issues/485)/[#486](https://github.com/pablof7z/nmp/issues/486) — [#531](https://github.com/pablof7z/nmp/pull/531).
- **Recent hardening batch (merged):** a permanently-failed-relay wedge + unbounded send queue fixed, three unbounded-memory bookkeeping structures pruned, wake-relay lane lookups indexed instead of full-scanned, store query indexes and batched GC landed, and kind-ownership exclusivity now has real enforcement (it was previously documented but unenforced — [#521](https://github.com/pablof7z/nmp/issues/521)).
- **Proven this session:** end-to-end relay ingest holds up at real scale — a real websocket-to-redb harness pushed 1,000,000 signed events through the actual transport/verifier/resolver path with exact persistence on reopen ([#535](https://github.com/pablof7z/nmp/pull/535), closes [#530](https://github.com/pablof7z/nmp/issues/530)). It also found the next gap honestly, and that gap is now closed too: the same run's peak RSS fell 803,774,464 → 122,511,360 bytes (-84.76%, +13.57% throughput) after bounding redb's page cache to an explicit 64 MiB ceiling ([#540](https://github.com/pablof7z/nmp/pull/540), closes [#534](https://github.com/pablof7z/nmp/issues/534)).
- **Headline (merged) — NIP-42 landed across six waves; issue #8 is closed. Protected *reads* are still broken against most deployed relays ([#1889](https://github.com/pablof7z/nmp/issues/1889)).** Content-relay authentication landed across six adversarially-reviewed waves this arc. Wave 1, access-scoped session identity ([#539](https://github.com/pablof7z/nmp/pull/539)), keyed relay identity/attribution/coverage/admission by `(relay, access)` instead of URL, closing the structural cross-account-credit gap (bug-class ledger #18) *before* any AUTH negotiation exists — passed an adversarial identity-isolation review clean. Wave 2, the AUTH reducer + epoch state machine ([#541](https://github.com/pablof7z/nmp/pull/541)), adds challenge epochs, a frozen `kind:22242` auth-event template, AUTH-OK kept structurally disjoint from a write ACK, and authenticated write sessions — an eight-invariant adversarial review found and fixed one real missed-wakeup, then re-verified clean. Wave 3, runtime capability binding + the real-WebSocket AUTH capstone ([#542](https://github.com/pablof7z/nmp/pull/542)), proves it against a real challenging relay: `challenge → policy → sign → AUTH → OK → REQ → EOSE → rows`, denial-parking, a fresh challenge on reconnect, and a wrong-challenge oracle — all 8 lifecycle/leak invariants passed adversarial review, "no correctness holes." Wave 5 ([#543](https://github.com/pablof7z/nmp/pull/543)) projected that onto the supported `nmp` facade: a registrable `AuthPolicy` trait, `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)` (closes [#495](https://github.com/pablof7z/nmp/issues/495)), and per-session auth diagnostics — all facade-owned rather than re-exported. Wave 6 ([#544](https://github.com/pablof7z/nmp/pull/544)) closed it out: the FFI + Swift + Kotlin projection — `NMPAuthPolicy`/`FfiAuthPolicy`, a resolve/cancel completion object, `auth_sessions` diagnostics, typed capability-exhaustion errors — passed a 7/7 adversarial race suite clean. Every wave was independently adversarially reviewed. Frame it honestly: an iOS/Android/desktop app can now register an `AuthPolicy`, resolve/deny relay challenges, do authenticated content-relay writes, and read per-session auth diagnostics — proven against a real strict-AUTH relay. Remaining, honestly: **protected reads deadlock against any relay that challenges in response to a request rather than unsolicited on connect ([#1889](https://github.com/pablof7z/nmp/issues/1889)), which includes strfry and so most deployed relays**; no standard Keychain/Keystore secure-signer providers yet, and an app-owned pending-cancel hook that never returns can still block engine shutdown (not AUTH-specific — see [known gaps](docs/known-gaps.md)); a macOS-only flake in the ingest-smoke suite ([#538](https://github.com/pablof7z/nmp/issues/538), closed via [#581](https://github.com/pablof7z/nmp/pull/581) — two real O_NONBLOCK-on-`accept()` races in the mock, not a product bug) is now fixed.
- **Superseded:** [`remove_account` (#529)](https://github.com/pablof7z/nmp/pull/529) was closed — its pubkey-only shape contradicted #8's ratified `AccountRegistration` model. Wave 5 replaced it with `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)`, which also closes [#495](https://github.com/pablof7z/nmp/issues/495).
- **Headline (merged) — architecture review is now enforced by CI, not just convention.** [#547](https://github.com/pablof7z/nmp/pull/547) closes [#496](https://github.com/pablof7z/nmp/issues/496): `AGENTS.md` gets a checked Noun / Reachability / Bool-Lifecycle / Destructive-API review-gate list (the exact discipline that caught `History*` but missed [#489](https://github.com/pablof7z/nmp/issues/489)), backed by a blocking CI job — cross-SDK parity (Swift/Kotlin FFI API must match Rust, modulo one documented exception). Backtested clean against 8 recent merged PRs / 43 named claims, and catches a fabricated claim plus a simulated #489-class regression.
- **Merged — signer hardening:** `LocalKeySigner`'s secret is now held in a `Zeroizing<[u8;32]>` with a redacted `Debug` impl ([#47](https://github.com/pablof7z/nmp/issues/47) Unit C, [#546](https://github.com/pablof7z/nmp/pull/546)) — the first landed unit of the broader signer-lifecycle epic. **Corrected since:** that `Zeroizing` field was an unused third copy while signing and NIP-44 still ran off a parallel long-lived `nostr::Keys`, so wiping it proved nothing about the operational secret. [#765](https://github.com/pablof7z/nmp/issues/765) removes the duplicate and makes the canonical zeroizing owner the only long-lived secret, with operation-scoped wiping owners for every derived key, hash/cipher state, and padded/decrypted plaintext.
- **Merged — #47 signer-lifecycle epic, Unit A:** per-write identity override across Rust/FFI/Swift/Kotlin ([#550](https://github.com/pablof7z/nmp/pull/550)) — publish under a registered secondary identity without moving `currentPubkey`; retarget-immunity is proven directly, including across a real redb close/reopen replay.
- **Merged — #47 signer-lifecycle epic, vault providers:** the secure-storage providers staged behind Unit A landed — a Keychain-backed account store (Swift, iOS/macOS) and a JVM `KeyStore`-backed account store (Kotlin/desktop), both restoring a session automatically ([#554](https://github.com/pablof7z/nmp/pull/554)).
- **Headline (merged) — #47 signer-lifecycle epic is complete; issue #47 is closed.** Unit B ([#556](https://github.com/pablof7z/nmp/pull/556)) carries the exact frozen pubkey on `WriteStatus::AwaitingCapability` so a parked write's stranded identity is observable, not just "still parked." Its own cross-API parity suite caught direct-Rust and FFI reattach reporting two genuinely *different* frozen pubkeys for the same receipt pre-merge — the review net catching a real bug before it shipped — was fixed, and merged clean. Combined with per-write override (Unit A, [#550](https://github.com/pablof7z/nmp/pull/550)), platform vault providers ([#554](https://github.com/pablof7z/nmp/pull/554)), and the earlier zeroize-hardening (Unit C, [#546](https://github.com/pablof7z/nmp/pull/546)), all four units are now merged across Rust/FFI/Swift/Kotlin and #47 is closed.
- **Headline (merged) — Blossom (#216) T15-A is complete end-to-end.** [#560](https://github.com/pablof7z/nmp/pull/560) closes [#555](https://github.com/pablof7z/nmp/issues/555): `nmp-ffi` takes `nmp-blossom` as a direct dependency and projects upload/mirror/delete/list to Swift (`Blossom.swift`) and Kotlin (`Blossom.kt`), with per-operation error enums mirroring every Rust taxonomy variant 1:1. Cross-SDK parity, `swift-package`, and `kotlin-package` CI all passed clean on merge — this was previously red on a real `[UInt8]`→`Data` mismatch, now fixed and verified, not just re-flaked. Combined with the merged core ([#552](https://github.com/pablof7z/nmp/pull/552)) and verbs ([#557](https://github.com/pablof7z/nmp/pull/557)), all three T15-A units are in: **Blossom media/blob is now callable from Rust, Swift, and Kotlin.** The owner also ruled on upload durability ([#559](https://github.com/pablof7z/nmp/issues/559) decision): ship standalone async upload now, with engine-integrated durable upload as an explicit additive upgrade later, not a non-goal — filed as [#562](https://github.com/pablof7z/nmp/issues/562).
- **Merged — NIP-68 picture events (T15-B):** `nmp-nip68` closes [#558](https://github.com/pablof7z/nmp/issues/558) ([#566](https://github.com/pablof7z/nmp/pull/566)) — builds a kind:20 draft with `imeta` images minted only from verified Blossom assets, plus a tolerant decoder. A same-day follow-up ([#568](https://github.com/pablof7z/nmp/pull/568)) threaded an explicit `created_at` through `build_picture` for determinism/FFI parity.
- **Headline (merged) — upload-then-publish composition (T15-C) is in; Blossom epic #216's core arc is complete.** [#575](https://github.com/pablof7z/nmp/pull/575) closes [#559](https://github.com/pablof7z/nmp/issues/559): a new `nmp-media` crate wires `prepare → upload → compose` as three witness-typed stages over the *existing* `publish()` path, with upload failure, publish failure, and success kept as three genuinely separate error types rather than one collapsed boolean. Combined with T15-A (Blossom core/verbs/SDK, [#552](https://github.com/pablof7z/nmp/pull/552)/[#557](https://github.com/pablof7z/nmp/pull/557)/[#560](https://github.com/pablof7z/nmp/pull/560)) and T15-B (NIP-68 picture schema, [#566](https://github.com/pablof7z/nmp/pull/566)/[#568](https://github.com/pablof7z/nmp/pull/568)), the full upload→picture-event→publish arc is proven end-to-end in Rust. Epic [#216](https://github.com/pablof7z/nmp/issues/216) stays open, honestly, for what's left: the batched FFI/Swift/Kotlin projection of NIP-68 + `nmp-media` (Rust-only today), durable upload as an additive upgrade ([#562](https://github.com/pablof7z/nmp/issues/562)), and BUD-03 server-list placement.
- **Fixed — NIP-77 subscription leak (follow-up to #570):** [#579](https://github.com/pablof7z/nmp/pull/579) closes an orphaned live-candidate leak in the negentropy live-EOSE-timeout fallback path, where a `limit:0` candidate REQ could linger unclosed and mint phantom coverage.
- **Merged — freshness axis on query demand:** [#577](https://github.com/pablof7z/nmp/pull/577) closes [#565](https://github.com/pablof7z/nmp/issues/565) — `MaxAge`/`CacheOnly` are now served directly from per-handle coverage watermarks.
- **Headline (merged) — explicit pre-signature write cancellation:** [#585](https://github.com/pablof7z/nmp/pull/585) closes [#533](https://github.com/pablof7z/nmp/issues/533) — an accepted-but-unsigned write used to be able to sit indefinitely with no receipt-keyed way to retract it. Now `cancel(receiptId)` is a durable, typed, idempotent operation across Rust/FFI/Swift/Kotlin: success atomically compensates the optimistic row, restores independently relay-observed displaced state but never obsolete unpublished local history, persists a `Cancelled` receipt fact in the same transaction, and releases in-flight signer ownership; a write that already crossed the signature boundary returns a precise typed refusal, never a silent no-op. Adversarial review caught and drove fixes for signer-task leaks, quarantined recovered writes, and signed-ephemeral replay before merge.
- **Merged — bounded ordinary row delivery under a slow consumer:** [#586](https://github.com/pablof7z/nmp/pull/586) replaces the per-observer unbounded `mpsc` channel with a one-slot mailbox — skipped reducer batches compose per event-id into one exact transition rebased onto the last delivered state, so a slow query consumer can no longer make the engine's memory grow or replay stale intermediate frames. Windowed rows/diagnostics already used one-slot latest snapshots; this closes the same gap for unwindowed ordinary delivery. Progresses [#46](https://github.com/pablof7z/nmp/issues/46) — receipt observation, graph/derived-set ceilings, relay-advertised limits, and scheduler/resource bounds stay open.
- **Content parsing no longer implies acquisition; locator decoding no longer
  chooses it either.** [#569](https://github.com/pablof7z/nmp/pull/569) made
  `nmp-content` parser-only. [#879](https://github.com/pablof7z/nmp/issues/879)
  corrects the remaining lower-layer coupling: core preserves exact `npub`,
  `nprofile`, `note`, `nevent`, and `naddr` values but exposes no generic
  demand planner. Swift reference components own independent visibility-scoped
  handles and ask an explicit app resolver for one ordinary demand; Kotlin
  mirrors the parser/locator boundary; one shared locator corpus proves
  Rust/FFI/Swift/Kotlin parity. There is no compatibility planner, shared
  mutable coordinator, or hydration count budget.
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

## Platforms in one line

Rust core is the truth · **Swift** behavior qualified on the macOS host (full Apple slices packaged on master; iOS runtime pending) · **Kotlin** desktop-JVM plus a source-reproducible feature-selected Android AAR qualified on a pinned API-35 emulator.

## Roadmap / where it's heading

- Govern the provisional demand / receipt / signer shapes toward a **v2 freeze**
- Encode lifecycle invariants **as types**, not conventions
- Close **platform qualification** — physical iOS runtime evidence and Android runtime/lifecycle/security qualification
- Finish **bounded delivery** with an explicit shortfall contract everywhere
- Broaden opt-in protocol modules without adding protocol-specific receipt or observation lifecycles
- Project NIP-68 + `nmp-media` composition through FFI/Swift/Kotlin, batched together (currently Rust-only)
- Revisit engine-integrated durable upload as an additive upgrade over standalone async upload ([#562](https://github.com/pablof7z/nmp/issues/562)), now that T15-C composition has landed
- **Shipped:** NIP-42 content-relay AUTH is wired end-to-end, Rust through Swift/Kotlin — all six waves merged, [#8](https://github.com/pablof7z/nmp/issues/8) closed. Authenticated writes work; **protected reads deadlock against relays that challenge in response to a request ([#1889](https://github.com/pablof7z/nmp/issues/1889))**. See Status / maturity above.
- **Shipped:** architecture-review discipline is now machine-enforced — cross-SDK parity runs as a blocking CI check ([#547](https://github.com/pablof7z/nmp/pull/547), closes [#496](https://github.com/pablof7z/nmp/issues/496)).
- **Shipped:** the **#47 signer-lifecycle epic is complete and closed** — zeroize-hardening, per-write identity override, reattachment with frozen-identity visibility, and Keychain/JVM-KeyStore vault providers all merged across Rust/FFI/Swift/Kotlin ([#546](https://github.com/pablof7z/nmp/pull/546)/[#550](https://github.com/pablof7z/nmp/pull/550)/[#556](https://github.com/pablof7z/nmp/pull/556)/[#554](https://github.com/pablof7z/nmp/pull/554)).
- **Shipped:** Blossom epic (#216) core arc — T15-A upload/mirror/delete/list + SDK projection ([#552](https://github.com/pablof7z/nmp/pull/552)/[#557](https://github.com/pablof7z/nmp/pull/557)/[#560](https://github.com/pablof7z/nmp/pull/560)), T15-B NIP-68 picture schema ([#566](https://github.com/pablof7z/nmp/pull/566)/[#568](https://github.com/pablof7z/nmp/pull/568)), and T15-C upload-then-publish composition ([#575](https://github.com/pablof7z/nmp/pull/575)) — callable end-to-end from Rust today; see Protocol modules above.
- **Shipped:** freshness axis on query demand — `MaxAge`/`CacheOnly` from coverage watermarks ([#577](https://github.com/pablof7z/nmp/pull/577), closes [#565](https://github.com/pablof7z/nmp/issues/565)).
- **Shipped:** explicit pre-signature write cancellation — receipt-keyed `cancel(receiptId)` across Rust/FFI/Swift/Kotlin, durable and idempotent ([#585](https://github.com/pablof7z/nmp/pull/585), closes [#533](https://github.com/pablof7z/nmp/issues/533)).
- **Shipped:** bounded ordinary row delivery under a slow consumer — one-slot rebased mailbox replaces the unbounded per-observer queue ([#586](https://github.com/pablof7z/nmp/pull/586), progresses [#46](https://github.com/pablof7z/nmp/issues/46)).
- **Shipped:** parser-only content plus component-owned reference acquisition —
  exact Rust locator values, app-resolved replaceable Swift loaders, Kotlin
  parser/locator parity, and one shared NIP-19 oracle
  ([#569](https://github.com/pablof7z/nmp/pull/569), corrected by
  [#879](https://github.com/pablof7z/nmp/issues/879)).

## The ownership boundary

| NMP owns | Your app owns | The UI framework owns |
|---|---|---|
| Canonical event & write-obligation storage | App state and architecture | Rendering and layout |
| Relay discovery, routing, sync, subscription lifecycle | Which queries and writes exist | Observation scope |
| Dedup, provenance, replacement, deletion, expiry | Account and identity experience | Navigation and presentation |
| Durable publication work and per-relay evidence | Ordering, moderation, product policy | Platform presentation details |
| Permanent diagnostics over all of the above | How evidence is explained to a person | — |

Diagnostics are a **permanent, read-only proof plane** — source plan, wire filters, connections, relay evidence, limits, write attempts — not a debug mode that changes behavior.

## Repo layout

- `crates/nmp` — the supported Rust facade (`nmp::Engine`); `crates/nmp-ffi` projects it to Swift/Kotlin via UniFFI
- `crates/nmp-{store,resolver,router,transport,signer}` — internal seams, not alternate APIs
- `crates/nmp-content` — optional parser-only semantic document layer
- `crates/nmp-{nip02,nip29,nip65,blossom,nip68,media}` — opt-in protocol modules
- `crates/nmp-outbox` — the NIP-65 outbox algorithm as an installable `AuthorRouteProvider`
- `Packages/NMP` (Swift) · `Packages/NMPKotlin` (Kotlin/JVM)
- `apps/Canary`, `apps/UIGallery` — SwiftUI proving grounds
- `docs/` — vision, design record, known gaps

## Start here

- [Builder guide](docs/builder/README.md) — product model, examples, platform guidance
- [Vision](docs/VISION.md) — north star and settled invariants
- [Known gaps](docs/known-gaps.md) — the honest built-vs-missing list
- [Contributor guide](AGENTS.md) — issue-first workflow and verification discipline

## Security & trust boundary

- NMP runs **in the host app** and owns local cache + write-obligation state.
- The app owns identity import, backup, removal, and user-facing trust policy.
- NMP exports one opaque, sensitive whole-session value; the app owns storing it. NMP does not ship a plaintext credential checkpoint.
- Key-handling and secure-signer production readiness is tracked openly in [known gaps](docs/known-gaps.md).

## Contributing

Every unit of work starts with a GitHub issue that captures why it matters. Read [`AGENTS.md`](AGENTS.md), then pick from the [open issues](https://github.com/pablof7z/nmp/issues).

## License

[MIT](LICENSE)
