# M3 — Store + transport + write outbox, durable: implementation plan

- **Date:** 2026-07-11
- **Status:** Provisional-until-v2 (no self-compat obligation). Builder-facing plan for M3 per `docs/VISION.md` §6.
- **Milestone:** M3 — make the engine **durable and network-real, still headless** (no FFI — that is M4). Persistent store behind the same single insert door; real WebSocket transport; durable write outbox; negentropy probing + neg-first sync; an engine-internal signer + encrypt/decrypt capability.
- **Gate:** running — integration suite against an in-process test relay (`nostr-relay-builder` `MockRelay` / `nak serve`), PLUS the preserved headless core tests. The ledger #5/#7/#8/#9 falsifiers + watermark-cold-start-offline + reconnection-replay ARE the pass criteria (§5).
- **Builds on:** M1 (`nmp-grammar`/`nmp-store`/`nmp-resolver`) + M2 (`nmp-router`). The pure sync core is unchanged in shape; M3 wraps it, it does not rewrite it.
- **Import gate:** transport, negentropy, store semantics, signer are HARVEST candidates from the old repo (`/Users/pablofernandez/Work/nostr-multi-platform`). Nothing crosses verbatim — §4 records what to READ and re-justify, per subsystem.

M3's kill is not thesis-level (VISION §6 M3): failures here are execution risk, fixed not abandoned. The one genuinely hard design question is the **sync-core / async-I/O seam** — resolved in §2, firmly, with one owner-note flagged.

---

## 1. Crate-layout delta + the sync/async seam

M2 is a four-crate workspace. M3 adds **three** crates and **extends** `nmp-store`. Few crates, YAGNI; each new crate is a distinct seam that buys builder parallelism and isolation-testability.

```
nostr, negentropy, redb, tungstenite/mio/rustls (external)
  ├── nmp-grammar    (M1) value types
  ├── nmp-store      (M1, EXTENDED) EventStore + MemoryStore + RedbStore
  │                     + provenance merge + coverage watermarks + claim-GC
  ├── nmp-resolver   (M1) graph engine → DemandDelta            (UNCHANGED)
  ├── nmp-router     (M2) demand → per-relay WireDelta          (UNCHANGED)
  ├── nmp-transport  (M3 NEW) generational WebSocket Pool; the async/threaded edge
  │                     deps: nmp-relay-url-ish, tungstenite/mio/rustls (feature `native`)
  ├── nmp-signer     (M3 NEW) SigningCapability + CryptoCapability + LocalKeySigner
  │                     deps: nostr (Keys/NIP-44); NO tokio (SignerOp poll model)
  └── nmp-engine     (M3 NEW) the runtime. THE sync/async seam.
        src/core/       EngineCore — the PURE synchronous reducer (headless)
        src/runtime/    EngineThread + Handle + transport wiring (the async edge)
        src/outbox/     write intents / receipts / durability class
        src/negentropy/ prober FSM + Reconciler (wraps `negentropy` crate)
        deps: grammar, store, resolver, router, transport, signer, negentropy(ext)
```

**Dependency direction.** Everything flows one way into `nmp-engine`; nothing depends on it. `nmp-transport` and `nmp-signer` depend on NO other NMP crate (pure edges → parallel builders, isolated tests). `nmp-store` gains dependencies only on `redb` (behind a `redb` feature) — the trait stays the seam so `MemoryStore` remains the test backend.

**Why not fold negentropy/signer into engine (YAGNI check):** the *pure* parts (reconciler FSM, probe state machine, local signer, encrypt/decrypt) are self-contained and headless-testable; splitting `nmp-signer` earns the remote-signer trait seam (NIP-46 later) and isolation; the negentropy reconciler is kept as an engine *module* (not a crate) because it is driven turn-by-turn by the reducer and shares the engine's message vocabulary — a crate boundary there would buy nothing. That is the line: signer = crate (clean capability seam), negentropy = module (reducer-coupled).

---

## 2. THE architectural answer: single owned engine thread + pure reducer; interior threading; no imposed runtime

**Position (firm).** `nmp-engine` is structured as two layers with a hard seam between them:

1. **`core` — a pure synchronous reducer.** `EngineCore` owns the M1 resolver `Engine<S>`, the M2 `Router`, the write-outbox state, the negentropy prober state, and the watermark reads. Its entire surface is one step function:

   ```rust
   impl EngineCore {
       pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
       pub fn tick(&mut self, now: Timestamp) -> Vec<Effect>;
   }
   ```

   `EngineMsg` and `Effect` are plain values (§3.4). `EngineCore` does **no I/O, spawns no threads, touches no socket, imposes no runtime, and is `!Send`-friendly** (it may keep the resolver's `Rc<RefCell<>>` exactly as M1 wrote it — it lives on one thread). This is the seam that **preserves M1/M2's headless property**: you test the whole engine's logic by feeding `EngineMsg`s and asserting `Effect`s, with zero network. It is v1's single-synchronous-actor kernel reducer, re-cut for the two-noun surface.

2. **`runtime` — the async edge.** `EngineThread` spawns **one dedicated OS thread** that runs a blocking `recv` loop over an `mpsc` inbox (D8: blocking recv / callbacks, never poll), calls `core.handle(msg)`, and dispatches the returned `Effect`s: `Wire`/`Replay` → `Pool::send`; `RequestSign`/`RequestDecrypt` → the signer capability; `EmitRows`/`EmitReceipt` → the app-facing sinks. `nmp-transport`'s `Pool` runs its **own** I/O threads (harvested `mio`-driven worker threads — **not tokio**); its `PoolEvent`s are translated to `EngineMsg::RelayFrame`/`RelayConnected`/`RelayDisconnected` and pushed onto the same inbox. A `Handle` (cheap, `Clone + Send`) is what the app holds: it sends command `EngineMsg`s in and registers row/receipt sinks.

**Why this model, and how it stays a library not a framework (P1).** The threading is **interior**: the engine spawns and owns its engine thread and the transport's I/O threads; **the app never sees tokio, never injects an executor, never adopts a runtime.** The app gets exactly the two nouns back as native values — row deltas on a sink, receipt status on a sink — which M4 wraps as `AsyncSequence`/`Flow`. Because the old repo's transport is already `mio`/`tungstenite` thread-based (NOT async-runtime-based), "impose no runtime" is *achievable by harvest*, not by invention. The two nouns cross the thread boundary as serializable values, consistent with §4's "never assume shared memory."

**Tradeoff.** One dedicated engine thread per `Engine` instance (an app has one engine — negligible), and the reducer→effects indirection is one layer more than calling the router inline. That layer is precisely what keeps I/O out of the core and the core headlessly testable — it is load-bearing, not ceremony.

**Owner-note (not a blocker, flagged for the record):** the *only* sub-decision I did not force is whether `nmp-transport` harvests the old repo's `mio`-based worker-thread pool **or** owns a private `tokio` runtime internally (still invisible to the app). I recommend **harvest the `mio` pool** — it already exists, is proven, and keeps the "no async runtime anywhere" property literally true. The alternative (interior tokio) is only worth it if re-justifying the `mio`/`rustls` stack against the new model proves costly. This is the biggest harvest-vs-rewrite call (§4.2); it does not change the reducer seam either way. If the owner wants to pre-empt it, a one-line ruling saves a spike.

---

## 3. Core types (sketches — fields + key signatures, not bodies)

### 3.1 `nmp-store` — persistent backend + watermark/provenance/GC (EXTENDED)

Backend pick: **`redb`** (pure-Rust embedded ACID key-value, single file, MVCC, no C toolchain/`build.rs`). Justification: the store's `query` is "candidate-set → `match_event`" (M1), not SQL — we need ordered key-value tables + a few indexes, not a query planner; redb gives durability with zero native-build friction and no `secp256k1`-style wasm landmine. Rejected: `rusqlite`/`sqlite` (C dep, SQL we don't use), `lmdb`/`heed` (C dep, unsafe env). Tradeoff: redb is younger than sqlite; mitigated by the trait seam (swap backends without touching callers) and by `MemoryStore` staying the reference oracle every persistent test diffs against.

```rust
/// Provenance is now a FIRST-CLASS field of the stored row (ledger #5), not a
/// sidecar. Merge-on-duplicate happens BEFORE any other insert processing.
pub struct Provenance { pub seen: BTreeMap<RelayUrl, Timestamp> }   // which relays, when
pub struct StoredEvent { pub event: nostr::Event, pub provenance: Provenance }

pub enum InsertOutcome {
    Inserted,
    Duplicate { provenance_grew: bool },      // M1's no-op stub becomes a real merge
    Superseded { replaced: EventId },
    Stale,
}

/// A coverage watermark: per (canonical filter hash, relay), the timestamp T
/// through which a sync has COMPLETED. Downward-closed: a row asserts [0, T].
/// "Presence is not coverage" — a row is written ONLY on EOSE (plain REQ) or
/// NEG-DONE (negentropy), NEVER from the mere presence of stored events.
pub struct CoverageRow { pub filter_hash: DescriptorHash, pub relay: RelayUrl, pub covered_through: Timestamp }

pub trait EventStore {
    fn insert(&mut self, event: nostr::Event, from: RelayObserved) -> InsertOutcome; // dedup→merge-prov→supersede
    fn query(&self, filter: &nostr::Filter) -> Vec<StoredEvent>;   // current winners, WITH provenance
    fn record_coverage(&mut self, hash: DescriptorHash, relay: &RelayUrl, covered_through: Timestamp);
    fn get_coverage(&self, hash: DescriptorHash, relay: &RelayUrl) -> Option<Timestamp>; // None = REFUSE floor
    fn gc(&mut self, claims: &ClaimSet) -> GcReport;               // claim-based bounded GC
}
pub struct RedbStore { /* by_id, addr_index, author_idx, kind_idx, tag_idx, coverage tbl */ }
pub struct MemoryStore { /* M1 store + provenance map + coverage map — updated in lockstep */ }

/// Claim = the union of every live query's demand skeletons (what a live handle
/// still needs). GC may evict only rows matched by NO claim; claimed rows and
/// all replaceable current-winners are retained. Bounded: cap + LRU within
/// unclaimed. (`ClaimSet` is derived from `resolver.active_demand()`.)
pub struct ClaimSet { /* set of ConcreteFilter skeletons */ }
```

Insert order (ledger #1 + #5, both inside the one door): dedup-by-id FIRST → on hit, **merge `from` into provenance and return `Duplicate{provenance_grew}` with no index churn**; else supersession (M1's newest-wins / lexical-tiebreak, unchanged). No public index/coverage setter beyond `record_coverage`, which only advances (never lowers, except eviction).

### 3.2 `nmp-transport` — generational Pool + health (HARVEST `nmp-network`)

```rust
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RelayHandle { slot: u32, generation: u64 }  // stale handle structurally rejected
pub enum WireFrame  { Text(String), Binary(Vec<u8>) }  // no "kind"/"pubkey" — substrate-grade
pub enum RelayFrame { Text(String), Auth(String) }     // wire pre-classifies AUTH only
pub enum PoolEvent {
    Connected { handle: RelayHandle, url: RelayUrl },   // NEW generation on reconnect
    Disconnected { slot: u32, reason: DisconnectReason },
    Frame { handle: RelayHandle, frame: RelayFrame },
    Health { slot: u32, health: RelayHealth },
}
pub struct RelayHealth { pub state: ConnState, pub backoff: Duration, pub last_rtt: Option<Duration> }

pub struct Pool { /* mio worker thread(s); one socket per canonical URL */ }
impl Pool {
    pub fn new(cfg: PoolConfig, sink: impl PoolEventSink) -> Self;
    pub fn ensure_open(&self, url: &RelayUrl) -> RelayHandle;     // reconnect bumps generation
    pub fn send(&self, h: RelayHandle, frame: WireFrame) -> bool; // false if h is stale
    pub fn close(&self, h: RelayHandle) -> bool;
    pub fn set_reconnect_preamble(&self, h: RelayHandle, frames: Vec<String>); // replay hook
    pub fn health(&self, h: RelayHandle) -> Option<RelayHealth>;
    pub fn shutdown(&self);
}
```

Structural properties (ledger #2/#3/#4 preserved at the wire): **push-model, no `send_to_all`** (the app iterates its plan; the engine iterates the router's `RelayPlan`); **generational handles** — a frame tagged with a superseded generation is dropped by the pool translator, and `send`/`close` on a stale handle returns `false`; bounded send/recv queues; per-relay reconnect with **jittered exponential backoff** + keepalive FSM.

### 3.3 `nmp-signer` — signing + co-located encrypt/decrypt (HARVEST `nmp-signer-iface`)

```rust
/// Pollable thunk (harvest SignerOp): an op that may complete later is polled
/// on the engine's blocking recv loop — NO tokio pulled into the engine (D8).
pub enum SignerOp<T> { Ready(Result<T, SignerError>), Pending(/* poll fn */) }

pub trait SigningCapability {
    fn public_key(&self) -> Option<Pubkey>;
    fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<SignedEvent>;
}
/// Co-located with the signer because the KEY LIVES IN THE ENGINE (ledger #12
/// M0 amendment: else identity-as-input breaks). Emits decrypted RAW tokens —
/// still zero presentation.
pub trait CryptoCapability {
    fn nip44_encrypt(&self, peer: Pubkey, plaintext: &str) -> SignerOp<String>;
    fn nip44_decrypt(&self, peer: Pubkey, ciphertext: &str) -> SignerOp<String>;
}
pub struct LocalKeySigner { keys: nostr::Keys }   // impls both; sufficient for M3
// Remote signer is a SEAM only: `trait RemoteSignerHandle` exists, NIP-46/55 NOT built (non-goal §7).
```

### 3.4 `nmp-engine` — the reducer vocabulary, outbox, prober

```rust
pub enum EngineMsg {
    Subscribe(LiveQuery, RowSink), Unsubscribe(HandleId),   // read noun
    SetActivePubkey(Option<Pubkey>),                        // identity = pure input (P3)
    Publish(WriteIntent, ReceiptSink),                      // write noun
    RelayConnected(RelayHandle, RelayUrl), RelayDisconnected(u32),
    RelayFrame(RelayHandle, RelayFrame),                   // EVENT/EOSE/OK/CLOSED/NEG-* parsed here
    SignerCompleted(ReceiptId, Result<SignedEvent, SignerError>),
    Tick(Timestamp),
}
pub enum Effect {
    Wire(WireDelta),                                       // → Pool::send per (relay, current handle)
    Replay(RelayUrl, Vec<WireReq>),                        // reconnect: resend current subs on NEW gen
    StartProbe(RelayUrl), NegOpen(ProbedRelay, ConcreteFilter),
    RecordCoverage(DescriptorHash, RelayUrl, Timestamp),
    EmitRows(HandleId, Vec<RowDelta>),                     // read result (raw rows + coverage)
    EmitReceipt(ReceiptId, WriteStatus),
    RequestSign(ReceiptId, UnsignedEvent), RequestDecrypt(EventId, Pubkey, String),
}

// ---- write outbox (intent plane, VISION §4/§9) ----
pub enum Durability { Durable, Ephemeral, AtMostOnce }    // M0 amendment; a typed property, not a noun
pub struct WriteIntent { pub unsigned: UnsignedEvent, pub durability: Durability, pub routing: WriteRouting }
pub enum WriteRouting {
    AuthorOutbox,                        // author's write relays (reuse router lanes)
    ToInboxes(Vec<Pubkey>),              // recipients' inboxes (kind:10050 / NIP-65 read)
    PrivateNarrow(PrivateRoute),         // ledger #6: narrow-only, fail-closed
}
pub struct PrivateRoute { relays: NarrowOnly<RelayUrl> }  // NO widen operation exists on this type
pub enum WriteStatus {                   // the receipt STREAM (never bool/void on the durable path)
    Accepted, AwaitingCapability, Signed(EventId), Routed(BTreeSet<RelayUrl>),
    Sent(RelayUrl), Acked(RelayUrl), Rejected(RelayUrl, String), GaveUp(RelayUrl),
}
pub struct Receipt { pub id: ReceiptId /* status arrives on the ReceiptSink */ }

// ---- negentropy prober (module) (HARVEST nmp-nip77) ----
pub enum ProbeState { Unknown, Probing, Supported, Unsupported }   // = RelayNegentropyState
/// Capability TOKEN (ledger #8): constructible ONLY from a `Supported` cache
/// entry. `NegOpen`/negentropy sync take `ProbedRelay`, never a bare RelayUrl —
/// an unprobed relay cannot reach the negentropy path; it gets a plain REQ.
pub struct ProbedRelay(RelayUrl);
pub struct Prober { states: HashMap<RelayUrl, ProbeState> }
```

The app-facing `Handle` exposes **only** `subscribe / unsubscribe / set_active_pubkey / publish` + sink registration — no open-REQ, no `relays:` parameter (ledger #2/#3 preserved by construction at the top edge too).

**Reducer flow (the spine).** `RelayFrame(EVENT)` → (decrypt if encrypted, via `RequestDecrypt`; else raw) → `store.insert(event, from=relay)` → `resolver.ingest` → `router.compile(active_demand)` → `Effect::Wire` + per-affected-handle `EmitRows` (local re-filter store rows through each handle's atoms, reusing M2 `deliver`). `RelayFrame(EOSE sub_id)` → `RecordCoverage`. `Publish(durable)` → persist intent → `Accepted` → `RequestSign` → `Signed` → route via router lanes → `Sent(relay)` per relay → on `OK` frame → `Acked(relay)`. `RelayConnected(new_gen)` → `Effect::Replay` of the current `RelayPlan` reqs for that relay.

---

## 4. Harvest-vs-rewrite, per subsystem (import gate; provenance recorded)

Nothing crosses verbatim. Each row: old file to READ, and what must be re-justified against the two-noun model.

| Subsystem | READ in old repo | Harvest (re-justify) | Rewrite fresh / why |
|---|---|---|---|
| **Transport pool** | `crates/nmp-network/src/pool/{mod,types,inner}.rs`, `relay_worker/{connect,socket_io,mod}.rs`, `relay_protocol.rs`, `keepalive.rs` | Generational `RelayHandle`, push-model (no send-to-all), backoff+jitter constants, keepalive FSM, reconnect-preamble replay hook — **operational lessons re-earned, not re-invented** (VISION §8) | Strip all `nmp-core`/kernel coupling; the `PoolEvent`↔`EngineMsg` translation is fresh (new reducer vocabulary). Keep `mio`, drop the actor-seam adapter. |
| **Negentropy** | `crates/nmp-nip77/src/{runtime,reconciler,filter,messages,codec}.rs` | `RelayNegentropyState` FSM (→`ProbeState`), `EligibleFilter` parse (search-unsupported etc.), `Reconciler` over the `negentropy` crate, the 30s liveness-deadline REQ fallback | The kernel "substrate seam" (outbound-REQ interceptor / inbound-text interceptor) is DROPPED — the reducer drives the prober directly. `ProbedRelay` token type is new (ledger #8 as a *type*, not a runtime check). |
| **Store semantics** | `crates/nmp-store/src/types/coverage.rs` (`CoverageRow`), `crates/nmp-core/src/kernel/coverage_ledger.rs` (`build_watermark_fn`, "no row ⇒ refuse the floor", "presence ≠ coverage") | The downward-closed watermark model, the refuse-to-floor rule, the eviction-lowers-`covered_through` invariant — **the whole ledger-#7 doctrine is already worked out; harvest the reasoning** | The redb table layout + candidate-index `query` is fresh (M1's `MemoryStore` is the oracle). Provenance-merge-in-insert is fresh (M1 stubbed it). |
| **Signer / crypto** | `crates/nmp-signer-iface/src/{op,signing,handle,nip44_session}.rs` | `SignerOp` poll-thunk (no-tokio), `SignedEvent`/`UnsignedEvent`, `RemoteSignerHandle` trait shape, NIP-44 session vocabulary | `CryptoCapability` co-location is fresh framing (M0 amendment). Local-key impl fresh. NIP-46 transport NOT harvested (non-goal). |
| **Write outbox** | `crates/nmp-core/src/publish/engine/{types,mod}.rs`, `kernel/publish_engine_terminals.rs` | Per-relay terminal model (`TerminalOutcome`, accepted/failed split), enqueue≠converged discipline | The `Durability` class + `WriteStatus` stream + `PrivateRoute` narrow-only type are fresh (M0 amendment / ledger #6 as types). Drop the action-ledger/correlation-id machinery (that was v1 app-framework). |

### 4.2 The biggest harvest-vs-rewrite call
The transport async model (§2 owner-note): **harvest the `mio` worker-thread pool** (keeps "no imposed runtime" literally true, reuses proven reconnection/backoff/keepalive) vs. **rewrite over an interior tokio runtime**. Recommendation: harvest `mio`. Everything else harvests cleanly at the reasoning level; transport is the one place where a large I/O stack crosses, so it is where the import gate bites hardest.

---

## 5. Tests — M3 pass criteria

Two tiers: **(A) headless core** — feed `EngineMsg`, assert `Effect` (zero I/O, preserves M1/M2); **(B) integration** — drive `EngineThread` + `Pool` against `MockRelay`/`nak serve`. Named with arrange/act/assert skeletons; ledger falsifiers flagged.

**Headless core (A):**
1. `ingest_frame_recompiles_wire_and_emits_rows` — *Arrange:* core with one `$myFollows` handle. *Act:* `handle(RelayFrame(EVENT kind:3))`. *Assert:* effects contain a `Wire(WireDelta)` opening the follows' atoms + `EmitRows` for the handle; no I/O touched.
2. `eose_records_coverage_watermark` (ledger #7 half) — *Act:* `RelayFrame(EOSE sub)`. *Assert:* exactly one `RecordCoverage(hash, relay, T)`; a non-EOSE EVENT batch records NONE (presence ≠ coverage).
3. `unprobed_relay_gets_req_not_negentropy` (ledger #8) — *Arrange:* relay `ProbeState::Unknown`. *Act:* sync plan. *Assert:* effect is a plain `Wire` REQ, never `NegOpen`. *Falsifier:* `NegOpen(RelayUrl, …)` does not compile — the arg is `ProbedRelay`, obtainable only from a `Supported` entry.
4. `enqueue_is_not_converged` (ledger #9) — *Act:* `Publish(durable)`. *Assert:* first `EmitReceipt` is `Accepted`, never a terminal; the Handle's publish method returns a `Receipt`, never `bool`/`()`. `Ephemeral` intent emits NO receipt; `AtMostOnce` sends once with no retry effect after a failure.
5. `private_route_fails_closed` (ledger #6) — *Arrange:* `PrivateNarrow` intent whose inbox is unresolvable. *Assert:* `Rejected`/typed error; NO `Wire` op to any public write relay. `PrivateRoute` exposes no widen method (compile-level).

**Integration (B):**
6. `reconnect_replays_current_subs` — *Arrange:* subscribe against a `MockRelay`; observe the REQ. *Act:* drop the connection; `MockRelay` accepts a reconnect (pool bumps generation). *Assert:* the current wire subs are REPLAYED on the new generation; a frame arriving tagged with the OLD generation is dropped (no delivery into a stale sub).
7. `stale_handle_rejected` (transport unit) — open, force reconnect (gen++), `send(old_handle)` → `false`; inbound old-gen frame dropped by the translator.
8. `provenance_merges_across_relays` (ledger #5) — *Arrange:* two `MockRelay`s both holding event E. *Act:* both deliver E. *Assert:* one stored row, `provenance.seen == {A:t1, B:t2}`, no index churn on the second insert (`Duplicate{provenance_grew:true}`). *Falsifier:* no `query` path returns a `StoredEvent` without its `provenance` field populated.
9. `watermark_cold_start_offline` (ledger #7, the headline) — *Arrange:* online, subscribe filter F against relay R; R returns 3 events + EOSE → watermark persisted to redb. *Act:* **shut the engine down, reopen it OFFLINE (no relays reachable)**, subscribe F again. *Assert:* the `EmitRows` coverage variant for R is `CompleteUpTo(watermark)` — an empty tail beyond the watermark is authoritative-empty; a DIFFERENT filter G with no row yields `Unknown`, never `Complete`. *Falsifier:* you cannot construct `CompleteUpTo` from a non-empty result — only `get_coverage` (a persisted EOSE/NEG-DONE) yields it.
10. `negentropy_first_then_req_fallback` — Supported relay → `NEG-OPEN` reconcile → NEG-DONE → coverage recorded; a relay that goes silent past the 30s liveness deadline → engine falls back to a plain REQ, coverage recorded from the EOSE. Unsupported relay never sees NEG-OPEN.
11. `write_ack_per_relay` — *Act:* publish durable intent to 2 `MockRelay`s, one OKs and one NACKs. *Assert:* receipt stream reaches `Acked(R_ok)` and `Rejected(R_bad, reason)`; durable retry/give-up observable; the answer to "is it sent?" is only readable from the stream.
12. `persistence_roundtrip` — insert events + record coverage; close redb; reopen; assert events (current winners only) + watermarks survived.
13. `gc_bounded_by_live_claims` — event matched by no live handle's demand is GC-eligible and evicted (eviction lowers any covering watermark per the harvested invariant); a claimed event and every replaceable current-winner are retained.
14. `two_nouns_only_at_the_handle` (structural) — assert the `Handle` public surface is exactly the four verbs + sink registration; grep-guard that no `relays:`/open-REQ method exists (ledger #2/#3 at the top edge).

Preserved: **all M1 contract tests + M2 property/differential/kill suites stay green** — M3 does not touch `nmp-resolver`/`nmp-router` behavior.

---

## 6. Build order for Sonnet builders

`‖` = parallel (disjoint crates). Each sub-milestone is independently green.

- **Step 0 — scaffold.** Add `nmp-transport`, `nmp-signer`, `nmp-engine` to the workspace; `redb`/`negentropy` deps; empty modules. *Green:* `cargo build`.
- **A1 ‖ — `nmp-store` persistence.** `RedbStore` behind `EventStore`; provenance-merge-in-insert; `CoverageRow` + `record/get_coverage`; claim-GC; `MemoryStore` updated in lockstep as the oracle. *Green:* 8, 12, 13 (store-level) + all M1 store tests.
- **A2 ‖ — `nmp-transport`.** Harvest the pool; generational handles, backoff+jitter, keepalive, health, reconnect-preamble. *Green:* 7 + transport unit suite.
- **A3 ‖ — `nmp-signer`.** `SigningCapability` + `CryptoCapability` + `LocalKeySigner` + `SignerOp`; `RemoteSignerHandle` seam. *Green:* signer unit suite (sign/verify, NIP-44 round-trip).
- **B — `nmp-engine::core` reducer.** `EngineMsg`/`Effect`; wire resolver+router+store; ingest→recompile→`Wire`+`EmitRows`; EOSE→coverage; row coverage variant. Depends on A1. *Green:* 1, 2, 9(headless half).
- **C — `nmp-engine::runtime`.** `EngineThread`, `Handle`, `PoolEvent`↔`EngineMsg` wiring, reconnection replay. Depends on B + A2. *Green:* 6, 14.
- **D — outbox.** Intents/receipts/`Durability`/`PrivateNarrow`; signing orchestration via A3; per-relay ack from `OK`. Depends on B + A3. *Green:* 4, 5, 11.
- **E — negentropy.** Prober FSM + `ProbedRelay` token + `Reconciler`; neg-first + REQ fallback; coverage from NEG-DONE. Depends on B + A2. *Green:* 3, 10.
- **F — integration harness + cold-start.** `MockRelay`/`nak serve` fixtures; the full falsifier suite incl. `watermark_cold_start_offline`. Depends on C+D+E. *Green:* 9 (full), full B-tier.

**Parallelism:** A1‖A2‖A3 fully parallel (disjoint crates, no NMP cross-deps). B waits on A1. C/D/E all fan out after B (C needs A2, D needs A3, E needs A2) and can run in parallel — three builders. F integrates last.

---

## 7. Explicit non-goals (defer list — do not gold-plate)

- **No FFI / SDK / Swift / Kotlin** — M4. Row deltas + receipts cross as plain values on sinks; `AsyncSequence`/`Flow` wrapping is M4.
- **No falsifier app** — M5.
- **No Collection observation mode / ordering / windowing / pagination / cursors** — M4+ (VISION §10). M3 delivers **raw row deltas + coverage**; it does not order or window them.
- **No multi-remote-signer breadth** — NIP-46/NIP-55 are a **seam** (`RemoteSignerHandle` trait) only; M3 ships `LocalKeySigner`. Remote-signer transport is later.
- **No wallet, no MLS/Marmot, no NIP-45 count.**
- **No web/wasm** — `redb` + `mio` transport are native; wasm is out of v2 (VISION §8) and the trait seams keep it possible later without API redesign.
- **No general filter lattice / new coalescing rules** — M2 settled coalescing; M3 consumes `WireDelta` as-is.
- **No always-on-vs-on-demand sync-loop policy decision** — M3 runs a small always-on engine thread (offline persistence wants it, VISION §8); a lazy "sync on demand" mode is a post-v2 knob, not an M3 surface change.
- **No AUTH/NIP-42 handshake driver** — the transport pre-classifies the `AUTH` frame (harvested), but the challenge/replay flow is deferred unless a falsifier test forces it.

---

## 8. The trickiest correctness risk + what's underspecified

**Watermark authority under coalescing (the load-bearing risk).** M2 coalesces per-element atoms into WIDE per-relay wire filters. An `EOSE`/`NEG-DONE` therefore proves completeness for the **wide wire filter's** shape over that relay — but a query's coverage question is about its **narrow atom**. Because widen-only guarantees `wide ⊇ atom`, wide-complete ⇒ atom-complete **for that relay**; but the watermark must be *keyed and attributed* so this derivation is exact, and a query's overall coverage must then **aggregate across its 2-relay-min covering set** (complete only when each covering relay is complete for the covering window). This is subtler than reconnection replay (the generational pool largely solves that) or ack semantics (mechanical from `OK` frames). Get the attribution key wrong and ledger #7 silently regresses to "non-empty = complete."

**Underspecified — flag for a crisp ruling (candidate Fable/owner consult if B stalls):**
1. **Coverage attribution key** — is `record_coverage` keyed by the WIDE wire-filter hash, the NARROW atom hash, or both, and what is the precise containment rule that lets a narrow query read completeness off a wide EOSE? This needs a written spec before B lands `EmitRows` coverage; it is the one genuinely non-mechanical M3 decision.
2. **Decrypt in the ingest path** — for `LocalKeySigner`, is `nip44_decrypt` executed synchronously inline (simplest) or as a `RequestDecrypt` effect that defers projection? M3 default: synchronous for the local key, but the stored-event type must not *assume* plaintext (M1 amendment). Confirm before E.
3. **GC vs watermark interaction** — the harvested rule lowers `covered_through` when a covered event is evicted; confirm the claim-GC never evicts an event still inside a proven window a live query depends on (or the watermark must lower in lockstep). Named in test 13; the exact ordering is worth a one-line ruling.

Everything else in M3 is execution risk — fixed, not escalated.
