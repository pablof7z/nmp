# NMP

**An embeddable Nostr client engine. You bring the app; NMP owns the network.**

A Rust library that packages the hard Nostr client machinery — relay routing, outbox discovery, canonical state, signing, durable publishing — behind a small API you *call*. Not a framework you live inside.

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

- **A live query** — a declarative demand ("these authors' notes"). NMP keeps the local view current, repairs relay work when inputs change, and streams you the result.
- **A write intent** — a durable publish obligation. NMP carries it through local acceptance, signing, routing, retry, and per-relay outcomes — and reports what it actually observed, not a misleading global-success boolean.

```text
YOUR APP  ── live queries / write intents ──▶  NMP  ──▶  Nostr relays & signers
 state · nav · identity · UI                 store · routing · delivery · diagnostics
```

## Build

With [Rust](https://www.rust-lang.org/tools/install) installed:

```bash
git clone https://github.com/pablof7z/nmp.git
cd nmp
cargo build -p nmp --release
```

`crates/nmp` is the supported facade. Everything else in `crates/` is either an
internal seam behind it or an opt-in protocol module you add on purpose.

## What you get today

Tags: ✅ built · 🧪 experimental / partial · ⛔ not yet

**Reading & state**
- ✅ Declarative live queries — a closed binding grammar with a reactive current-pubkey root, derived projections over stored Nostr state, and set algebra
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
- ✅ **NIP-42 AUTH — content-relay authentication, wired end-to-end; authenticated reads and writes both work, against relays that challenge unsolicited on connect and against relays that challenge in response to a request** ([#8](https://github.com/pablof7z/nmp/issues/8) and [#1889](https://github.com/pablof7z/nmp/issues/1889), both closed). Several adversarially-reviewed waves landed it. The first keys relay identity/attribution/coverage/admission by **session, not URL** (`AccessContext { Public, Nip42(pubkey) }` + `RelaySessionKey`), passing an adversarial identity-isolation review clean ([#539](https://github.com/pablof7z/nmp/pull/539)). The second adds the **AUTH reducer + epoch state machine**: challenge epochs, a frozen `kind:22242` auth-event template (id commits to every field), AUTH-OK kept structurally disjoint from a durable write ACK, and authenticated write sessions — an eight-invariant adversarial review caught and fixed a real missed-wakeup, then re-verified clean ([#541](https://github.com/pablof7z/nmp/pull/541)). The third adds **runtime capability binding** (`AuthPolicy` trait, bounded registry, `Handle::{add,remove}_auth_policy`) and a **real-WebSocket AUTH capstone**: a strict relay proves `challenge → policy → sign → AUTH → OK → REQ → EOSE → rows` end-to-end, plus denial-parking, a fresh challenge on reconnect, and a wrong-challenge oracle — all 8 lifecycle/leak invariants passed adversarial review clean ([#542](https://github.com/pablof7z/nmp/pull/542)). The fourth projects that onto the **app-facing facade**: a registrable `AuthPolicy` trait, `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)` (closes [#495](https://github.com/pablof7z/nmp/issues/495)), and per-session auth diagnostics — the snapshot records **facade-owned** rather than re-exported ([#543](https://github.com/pablof7z/nmp/pull/543)), though the closed AUTH phase vocabulary itself is one engine-owned type re-exported to every surface ([#1616](https://github.com/pablof7z/nmp/issues/1616)). Net result: an app can register an `AuthPolicy`, resolve or deny a relay's challenge, do authenticated content-relay writes, and read per-session auth diagnostics — proven against a real strict-AUTH relay with a non-vacuous wrong-challenge oracle. [#1889](https://github.com/pablof7z/nmp/issues/1889) closed the last gap: a protected read used to withhold its REQ until AUTH completed while the relay withheld its challenge until it saw a request, so against strfry — and so against most deployed relays — the query never transmitted a byte and the installed `AuthPolicy` was never consulted. A read session now sends its REQ whether or not it names an identity, and answers the challenge that provokes; the whole round trip, the re-AUTH after a reconnect, and an app-refused handshake were driven against a real strfry process. Honest remaining gaps: no secure-signer provider implementations ship with the engine (see Signing & identity below); and engine shutdown can still block on an app-owned pending-cancel hook that never returns — an app-hook contract issue, not specific to AUTH (see [known gaps](docs/known-gaps.md))

**Signing & identity**
- ✅ Local key signer — one fixed-allocation, non-`Clone` canonical zeroizing secret owner (moving the signer relocates only its pointer), with operation-scoped BIP-340/NIP-44 secret, key, hash-state, cipher-state, and plaintext owners that wipe on success, refusal, and unwind; no operational `nostr::Keys`/`SecretKey`/`Keypair` is retained, and `Debug` is redacted to the public key only ([#546](https://github.com/pablof7z/nmp/pull/546) began this; [#765](https://github.com/pablof7z/nmp/issues/765) replaced its unused duplicate with the real operational owner)
- ✅ Per-write identity override — publish a single write under a secondary session account without changing the current account. Retarget-immunity is proven: once accepted under the override, a later account switch can never redirect it to a different signer, even across a store close/reopen ([#47](https://github.com/pablof7z/nmp/issues/47) Unit A, [#550](https://github.com/pablof7z/nmp/pull/550))
- ✅ Whole-session account model — signer-backed and public-key-only accounts, optional current selection, and provider reconstruction material export and restore as one opaque value. Provider reachability is runtime state, never a reason to drop the account from the restored session ([#1397](https://github.com/pablof7z/nmp/issues/1397))
- ✅ Frozen identity on a parked write (`AwaitingCapability{pubkey}`) — a stranded reattached write now carries the exact pubkey it's still waiting on, not just "still parked" ([#47](https://github.com/pablof7z/nmp/issues/47) Unit B, [#556](https://github.com/pablof7z/nmp/pull/556))
- ⛔ No secure-storage signer providers ship with the engine; an app owns storing the opaque session value under its own platform policy. App-owned transactional session storage remains tracked separately in [#1398](https://github.com/pablof7z/nmp/issues/1398)

**Publishing**
- ✅ **Durable write intents** — `Accepted` is one atomic persistence boundary (frozen body, receipt, pending row visible to queries)
- ✅ **Replaceable delivery coalescing and disposal** — a newer kind `0`, `3`, `10000...19999`, or same-`d` `30000...39999` write destroys the older event body, route, lanes, attempts, and deadlines instead of replaying obsolete bytes. Work proved never handed off leaves no receipt; possible-handoff ambiguity keeps only a typed `Superseded` safety receipt in the same internally bounded terminal history as every other completed write. Already-expired writes are refused before custody and retain nothing.
- ✅ **Explicit pre-signature write cancellation** — `Engine::cancel(ReceiptId)` atomically compensates the optimistic row, restores a relay-observed displaced predecessor when one exists, never resurrects obsolete unpublished local history, persists a durable `Cancelled` receipt fact, and cancels in-flight signer work. Idempotent; a write that already signed returns a precise typed refusal instead of silently no-op'ing ([#533](https://github.com/pablof7z/nmp/issues/533) closed, [#585](https://github.com/pablof7z/nmp/pull/585))
- ✅ Signature promotion, internal-failure cancellation + compensation, persisted **bounded-retry delivery** (32 global / 1 per relay, deterministic backoff)
- ✅ At-most-once ambiguity becomes `OutcomeUnknown` — never a blind resend
- ✅ Verbatim publish of externally pre-signed events

**Protocol modules** (opt-in — core stays kind-agnostic)
- ✅ NIP-02 following — durable tag-preserving follow/unfollow over cached,
  first-value, and later relay source truth, with one ordinary receipt
- ✅ NIP-65 — `nmp-nip65` holds engine-free kind:10002 values
  (validation, composition, canonical winners, marker parsing); `nmp-outbox`
  turns them into an installable `AuthorRouteProvider`. Cold-product capstones
  prove the configured indexer discovers the outbox, the write reaches only
  that learned relay, and no undeclared relay is contacted.
- ✅ NIP-73 external content ids — the `(i, k)` pair naming something that is
  not a Nostr event, in its own crate because several NIPs consume them and
  none owns them. Podcast episodes, `web` URLs (canonicalised: normalised, no
  fragment), and an already-canonical general pair.
- ✅ NIP-22 comments over NIP-73 external content ids — typed root/parent
  validation, thread demand, decode, and deterministic composition.
  Composition is an engine-free protocol function returning the ordinary
  `WriteIntent`; apps publish it through the one generic `publish` → `Receipt`
  lifecycle.
- ✅ Optional parser-only content module (source-ranged plaintext/Markdown and
  NIP-19 occurrences) with exact five-variant locator values. Core decoding
  owns no kind:0, source-authority, relay-admission, or hidden fan-out policy;
  exact kind:0/NIP-23 codecs belong to their own optional protocol owners
  ([#561](https://github.com/pablof7z/nmp/issues/561), corrected by
  [#879](https://github.com/pablof7z/nmp/issues/879))
- 🧪 NIP-29 groups — a group can live on more than one relay, so the API is
  relay-scoped: `nmp_nip29::on(hosts)` returns a `RelayScope` (fallible — an
  app-supplied relay set can be empty), narrowed to one `Group` via
  `.group(id)` ([#1033](https://github.com/pablof7z/nmp/issues/1033);
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
  removed because C7 owns chat and `q` replies. The public API was exercised
  against two real local relays.
- 🧪 NIP-29 remembered-groups product capability — `nmp-nip29` exposes observational reading of NIP-51 kind:10009, while `nmp::nip29` owns typed group and relay-in-use add/remove operations through the ordinary durable semantic-write receipt ([#1552](https://github.com/pablof7z/nmp/issues/1552))
- ⛔ No NIP-25 reactions, no general draft composition

**Storage**
- ✅ Crash-safe redb: binary canonical rows, secondary + tag + cardinality indexes, interned relay URLs
- ✅ Destructive reset that structurally **refuses to delete a live store**
- 🧪 Cross-process reset exclusion (no advisory/sidecar lock yet)

## Status / maturity

- **Pre-1.0, pre-v2.** The v2 *public API is freezing*; public names and shapes are provisional.
- **Rust-only.** The Swift, Kotlin, Android, UniFFI/FFI and native-packaging estate was deleted; NMP is one Rust workspace with `crates/nmp` as its facade.
- **The reference application is `crates/nmp-canary`** — a NIP-29 rooms client written against the public surface, whose job is to keep the API honest. Its findings are the deliverable, printed as ranked data by `cargo run -p nmp-canary --bin canary findings`. It exercises every surface against a real engine and a real store; the relay half is a separate harness that does not exist yet. Charter: [`docs/internals/reference-app.md`](docs/internals/reference-app.md).
- **Pending:** several guarantees remain active work — see [`docs/known-gaps.md`](docs/known-gaps.md) (honest built-vs-missing record) and the [structural guarantees](docs/builder/28-patterns.md) (what the design excludes, and how).
- The ownership boundary and behavioral invariants are the stable frame; the app-facing spelling is not.
- **Headline (merged):** history is no longer a second noun — `observe(query, window)` makes windowing a policy on the one read noun, delivery mode derives from boundedness, and the #486 per-advance relay-REQ leak is fixed (deep scroll now holds O(1) live subscriptions per relay). Closes [#474](https://github.com/pablof7z/nmp/issues/474)/[#485](https://github.com/pablof7z/nmp/issues/485)/[#486](https://github.com/pablof7z/nmp/issues/486) — [#531](https://github.com/pablof7z/nmp/pull/531).
- **Recent hardening batch (merged):** a permanently-failed-relay wedge + unbounded send queue fixed, three unbounded-memory bookkeeping structures pruned, wake-relay lane lookups indexed instead of full-scanned, store query indexes and batched GC landed, and kind-ownership exclusivity now has real enforcement (it was previously documented but unenforced — [#521](https://github.com/pablof7z/nmp/issues/521)).
- **Measured:** end-to-end relay ingest holds up at real scale — a real websocket-to-redb harness pushed 1,000,000 signed events through the actual transport/verifier/resolver path with exact persistence on reopen ([#535](https://github.com/pablof7z/nmp/pull/535), closes [#530](https://github.com/pablof7z/nmp/issues/530)). It also found the next gap honestly, and that gap is now closed too: the same run's peak RSS fell 803,774,464 → 122,511,360 bytes (-84.76%, +13.57% throughput) after bounding redb's page cache to an explicit 64 MiB ceiling ([#540](https://github.com/pablof7z/nmp/pull/540), closes [#534](https://github.com/pablof7z/nmp/issues/534)).
- **Headline (merged) — NIP-42 landed; issues #8 and [#1889](https://github.com/pablof7z/nmp/issues/1889) are both closed. Authenticated reads and writes work against real deployed relays.** Access-scoped session identity ([#539](https://github.com/pablof7z/nmp/pull/539)) keyed relay identity/attribution/coverage/admission by `(relay, access)` instead of URL, closing the structural cross-account-credit gap (guarantee #18) *before* any AUTH negotiation existed — passed an adversarial identity-isolation review clean. The AUTH reducer + epoch state machine ([#541](https://github.com/pablof7z/nmp/pull/541)) adds challenge epochs, a frozen `kind:22242` auth-event template, AUTH-OK kept structurally disjoint from a write ACK, and authenticated write sessions — an eight-invariant adversarial review found and fixed one real missed-wakeup, then re-verified clean. Runtime capability binding + the real-WebSocket AUTH capstone ([#542](https://github.com/pablof7z/nmp/pull/542)) proves it against a real challenging relay: `challenge → policy → sign → AUTH → OK → REQ → EOSE → rows`, denial-parking, a fresh challenge on reconnect, and a wrong-challenge oracle — all 8 lifecycle/leak invariants passed adversarial review, "no correctness holes." [#543](https://github.com/pablof7z/nmp/pull/543) projected that onto the supported `nmp` facade: a registrable `AuthPolicy` trait, `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)` (closes [#495](https://github.com/pablof7z/nmp/issues/495)), and per-session auth diagnostics — all facade-owned rather than re-exported. Every wave was independently adversarially reviewed. A later change closed the last hole: protected reads used to deadlock against any relay that challenges in response to a request rather than unsolicited on connect ([#1889](https://github.com/pablof7z/nmp/issues/1889)), which included strfry and so most deployed relays — a read session now transmits its REQ whether or not it names an identity, and the full round trip was driven against a real strfry process. Remaining, honestly: no secure-signer providers ship with the engine, and an app-owned pending-cancel hook that never returns can still block engine shutdown (not AUTH-specific — see [known gaps](docs/known-gaps.md)).
- **Superseded:** [`remove_account` (#529)](https://github.com/pablof7z/nmp/pull/529) was closed — its pubkey-only shape contradicted #8's ratified `AccountRegistration` model. [#543](https://github.com/pablof7z/nmp/pull/543) replaced it with `add_account -> AccountRegistration` / `remove_account(&AccountRegistration)`, which also closes [#495](https://github.com/pablof7z/nmp/issues/495).
- **Merged — signer hardening:** `LocalKeySigner`'s secret is now held in a `Zeroizing<[u8;32]>` with a redacted `Debug` impl ([#47](https://github.com/pablof7z/nmp/issues/47) Unit C, [#546](https://github.com/pablof7z/nmp/pull/546)) — the first landed unit of the broader signer-lifecycle epic. **Corrected since:** that `Zeroizing` field was an unused third copy while signing and NIP-44 still ran off a parallel long-lived `nostr::Keys`, so wiping it proved nothing about the operational secret. [#765](https://github.com/pablof7z/nmp/issues/765) removes the duplicate and makes the canonical zeroizing owner the only long-lived secret, with operation-scoped wiping owners for every derived key, hash/cipher state, and padded/decrypted plaintext.
- **Headline (merged) — #47 signer-lifecycle epic is complete; issue #47 is closed.** Unit A ([#550](https://github.com/pablof7z/nmp/pull/550)) is per-write identity override: publish under a registered secondary identity without moving the current pubkey, with retarget-immunity proven directly, including across a real redb close/reopen replay. Unit B ([#556](https://github.com/pablof7z/nmp/pull/556)) carries the exact frozen pubkey on `WriteStatus::AwaitingCapability` so a parked write's stranded identity is observable, not just "still parked." Combined with the earlier zeroize-hardening (Unit C, [#546](https://github.com/pablof7z/nmp/pull/546)), #47 is closed. The platform vault providers that landed under it ([#554](https://github.com/pablof7z/nmp/pull/554)) were Keychain- and JVM-KeyStore-backed and were deleted with the native estate; an app now owns storing the opaque session value itself.
- **Merged — freshness axis on query demand:** [#577](https://github.com/pablof7z/nmp/pull/577) closes [#565](https://github.com/pablof7z/nmp/issues/565) — `MaxAge`/`CacheOnly` are now served directly from per-handle coverage watermarks.
- **Headline (merged) — explicit pre-signature write cancellation:** [#585](https://github.com/pablof7z/nmp/pull/585) closes [#533](https://github.com/pablof7z/nmp/issues/533) — an accepted-but-unsigned write used to be able to sit indefinitely with no receipt-keyed way to retract it. Now `cancel(receiptId)` is a durable, typed, idempotent operation: success atomically compensates the optimistic row, restores independently relay-observed displaced state but never obsolete unpublished local history, persists a `Cancelled` receipt fact in the same transaction, and releases in-flight signer ownership; a write that already crossed the signature boundary returns a precise typed refusal, never a silent no-op. Adversarial review caught and drove fixes for signer-task leaks, quarantined recovered writes, and signed-ephemeral replay before merge.
- **Merged — bounded ordinary row delivery under a slow consumer:** [#586](https://github.com/pablof7z/nmp/pull/586) replaces the per-observer unbounded `mpsc` channel with a one-slot mailbox — skipped reducer batches compose per event-id into one exact transition rebased onto the last delivered state, so a slow query consumer can no longer make the engine's memory grow or replay stale intermediate frames. Windowed rows/diagnostics already used one-slot latest snapshots; this closes the same gap for unwindowed ordinary delivery. Progresses [#46](https://github.com/pablof7z/nmp/issues/46) — receipt observation, graph/derived-set ceilings, relay-advertised limits, and scheduler/resource bounds stay open.
- **Content parsing no longer implies acquisition; locator decoding no longer
  chooses it either.** [#569](https://github.com/pablof7z/nmp/pull/569) made
  `nmp-content` parser-only. [#879](https://github.com/pablof7z/nmp/issues/879)
  corrects the remaining lower-layer coupling: core preserves exact `npub`,
  `nprofile`, `note`, `nevent`, and `naddr` values but exposes no generic
  demand planner. There is no compatibility planner, shared mutable
  coordinator, or hydration count budget.
- **Also open:** a consolidated **v2 architecture decision record** ([#548](https://github.com/pablof7z/nmp/issues/548), 15 rulings against standing doctrine) — now published as a browsable page with a spoken overall briefing plus a per-issue deep-dive: [pablof7z.github.io/nmp/v2-escalation](https://pablof7z.github.io/nmp/v2-escalation/).

## Performance

Built for **bounded memory and streaming — never first-N truncation.** Measured on a real ~1,100-event corpus / million-row fixture:

- Busiest-room query: **5.15 ms → 0.26 ms**
- Derived-set resolver over a **59,915-row** bucket: **3,786 ms → 0.73 ms**
- Rejected-heavy search: **0.188 ms → 0.005 ms**
- Router coalesce fixed-point: **O(n³) → O(n²)**, plan-identical output
- Ordinary query delivery to a slow observer is now bounded by a one-slot rebased mailbox instead of an unbounded per-update queue — memory tracks the semantic delta since last delivery, not the number of missed updates ([#586](https://github.com/pablof7z/nmp/pull/586))
- Query planning picks one best index and **stops at the visible limit** — no full-history materialization
- **Relay ingest measured end-to-end at real scale** — 1,000,000 signed events over the actual websocket/transport/verifier/resolver/redb path, all frames accounted for and exactly recovered on reopen: ~4,333 events/s, 4.96s p95 apply latency, 2.08 GB store ([#535](https://github.com/pablof7z/nmp/pull/535), closes [#530](https://github.com/pablof7z/nmp/issues/530)). Peak RSS during that same run is now bounded too — an explicit 64 MiB redb page-cache ceiling cut it 803,774,464 → 122,511,360 bytes (-84.76%), with +13.57% throughput ([#540](https://github.com/pablof7z/nmp/pull/540), closes [#534](https://github.com/pablof7z/nmp/issues/534))
- NIP-11 cache carries a **proven ~67 MiB raw-body ceiling** (not a total-RSS claim)

## Roadmap / where it's heading

- Govern the provisional demand / receipt / signer shapes toward a **v2 freeze**
- Encode lifecycle invariants **as types**, not conventions
- Finish **bounded delivery** with an explicit shortfall contract everywhere
- Broaden opt-in protocol modules without adding protocol-specific receipt or observation lifecycles
- Build the Rust reference application that keeps the public API honest
- **Shipped:** NIP-42 content-relay AUTH is wired end-to-end, [#8](https://github.com/pablof7z/nmp/issues/8) closed. Authenticated reads and writes both work, including against relays that only challenge in response to a request ([#1889](https://github.com/pablof7z/nmp/issues/1889) closed). See Status / maturity above.
- **Shipped:** the **#47 signer-lifecycle epic is complete and closed** — zeroize-hardening, per-write identity override, and reattachment with frozen-identity visibility ([#546](https://github.com/pablof7z/nmp/pull/546)/[#550](https://github.com/pablof7z/nmp/pull/550)/[#556](https://github.com/pablof7z/nmp/pull/556)).
- **Shipped:** freshness axis on query demand — `MaxAge`/`CacheOnly` from coverage watermarks ([#577](https://github.com/pablof7z/nmp/pull/577), closes [#565](https://github.com/pablof7z/nmp/issues/565)).
- **Shipped:** explicit pre-signature write cancellation — receipt-keyed `cancel(receiptId)`, durable and idempotent ([#585](https://github.com/pablof7z/nmp/pull/585), closes [#533](https://github.com/pablof7z/nmp/issues/533)).
- **Shipped:** bounded ordinary row delivery under a slow consumer — one-slot rebased mailbox replaces the unbounded per-observer queue ([#586](https://github.com/pablof7z/nmp/pull/586), progresses [#46](https://github.com/pablof7z/nmp/issues/46)).
- **Shipped:** parser-only content plus exact locator values
  ([#569](https://github.com/pablof7z/nmp/pull/569), corrected by
  [#879](https://github.com/pablof7z/nmp/issues/879)).

## The ownership boundary

| NMP owns | Your app owns |
|---|---|
| Canonical event & write-obligation storage | App state and architecture |
| Relay discovery, routing, sync, subscription lifecycle | Which queries and writes exist |
| Dedup, provenance, replacement, deletion, expiry | Account and identity experience |
| Durable publication work and per-relay evidence | Ordering, moderation, product policy |
| Permanent diagnostics over all of the above | Rendering, navigation, and how evidence is explained to a person |

Diagnostics are a **permanent, read-only proof plane** — source plan, wire filters, connections, relay evidence, limits, write attempts — not a debug mode that changes behavior.

## Repo layout

- `crates/nmp` — the supported Rust facade (`nmp::Engine`)
- `crates/nmp-{engine,runtime}` — the deterministic reducer and the async edge that interprets its effects
- `crates/nmp-{store,resolver,router,transport,signer,local-signer,grammar}` — internal seams, not alternate APIs
- `crates/nmp-content` — optional parser-only semantic document layer
- `crates/nmp-{nip02,nip11,nip18,nip22,nip25,nip29,nip65,nipc7,nip73,bookmarks}` — opt-in protocol modules
- `crates/nmp-outbox` — the NIP-65 outbox algorithm as an installable `AuthorRouteProvider`
- `docs/` — vision, design record, known gaps

## Start here

- [Builder guide](docs/builder/README.md) — product model and examples
- [Vision](docs/VISION.md) — north star and settled invariants
- [Known gaps](docs/known-gaps.md) — the honest built-vs-missing list
- [Contributor guide](AGENTS.md) — cold-start reading order and working discipline

## Security & trust boundary

- NMP runs **in the host app** and owns local cache + write-obligation state.
- The app owns identity import, backup, removal, and user-facing trust policy.
- NMP exports one opaque, sensitive whole-session value; the app owns storing it. NMP does not ship a plaintext credential checkpoint.
- Key-handling and secure-signer production readiness is tracked openly in [known gaps](docs/known-gaps.md).

## Contributing

Every unit of work starts with a GitHub issue that captures why it matters. Read [`AGENTS.md`](AGENTS.md), then pick from the [open issues](https://github.com/pablof7z/nmp/issues).

## License

[MIT](LICENSE)
