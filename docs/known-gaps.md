# Known gaps & deferred follow-ups

Honest running list of things built-but-incomplete or deliberately deferred, so nothing hides. Each says who flagged it, why it's deferred, and when it must be closed. This is a truth-anchor companion to the bug-class ledger.

## Promoted v2 contract gaps - next work frame

The July 11 architecture promotion intentionally corrected several assumptions
after the original milestones. These are agreed target contracts, not claims
about current code:

- **Demand is still filter-shaped.** The supported descriptor does not yet carry
  `selection + source authority + access context` end to end. Hashing,
  coalescing, routing, evidence, FFI, Swift, and Kotlin must project the same
  semantic descriptor. See `docs/design/query-demand-and-evidence.md`.
- **Scoped acquisition evidence is built, but its reserved transport states
  are not populated yet.** Query snapshots now carry rows plus compact
  per-current-plan source facts and explicit shortfalls; diagnostics retains
  exact per-relay/filter intervals as a distinct type. `AwaitingAuth` and
  `AuthDenied` land with #8, and `Error` lands with #51. Full source-authority
  and access-context identity remains part of the descriptor gap above (#49).
- **Whole-demand relay admission and fan-out limiting are built (#20).** One
  finite ceiling now covers the fully assembled read plan and the live
  transport worker set, including outbox, indexer, app, fallback, and explicit
  pinned lanes. Deterministic plan-time refusals are absent from executable
  wire work but remain visible as exact contextual `LocalLimit` query evidence
  and diagnostics; the transport boundary returns typed admission errors and
  preserves durable write lanes as explicit waiting work. Runtime
  reconciliation releases workers that have no current read, write, or
  ephemeral owner before dialing replacements, while nonterminal durable
  lanes retain their shared socket ownership. Discovered local,
  private, link-local, and `.onion` targets remain rejected by default, while
  operator-configured and explicit contextual authority keep their separate
  trusted path. Other non-relay limit classes remain open under ledger #17.
- **Crash-safe acceptance, restart reattachment, bounded retry, and governed
  SDK observation are built.** One transaction
  owns the intent, stable receipt,
  frozen body, canonical pending row and displaced state. Rust boot recovery
  rebuilds ownership without reinsertion, resumes the frozen signer, persists
  versioned exact-byte `(intent, relay, ordinal)` Started facts before wire,
  replays Durable in-flight bytes, and converts AtMostOnce ambiguity to
  `OutcomeUnknown` without resend. A failure to persist `Started` retains the
  relay as an explicit nonterminal owned lane, emits `PersistenceBlocked`
  without emitting an untracked wire EVENT. Exact dynamically-resolved relay
  sets are first committed as append-only route revisions, so restart owns
  their union even if the live directory is empty or changed. Failure to
  persist the route revision is reported separately and makes no false claim
  that the exact URL survived a crash. One typed engine reducer now owns stable
  due ordering, 32-global/1-per-relay caps, deterministic 3s-to-300s backoff,
  30s ACK expiry, handoff and standardized relay-result classification, and
  every eligibility transition. Offline and AUTH waits allocate no attempt and
  arm no polling deadline; Durable retry advances the persisted ordinal, while
  AtMostOnce ambiguity becomes terminal `OutcomeUnknown`. The deadline exposed
  by `next_deadline()` is consumed before it can be rearmed, including bounded
  batch draining, so there is no zero-timeout busy-spin. Rust, UniFFI, Swift,
  and Kotlin receipts distinguish relay/AUTH waits, retry eligibility with the
  persisted attempt ordinal and time, ambiguous handoff, and proven socket
  write/flush persisted against an exact lane ordinal; `Sent` is never emitted
  for queue acceptance, ambiguity, or an ephemeral handoff with no outbox fact.
- **The NIP-46 reconnect and governed sign-only paths are built; standard platform vault providers are not.**
  A current NIP-46 client now owns its independent signer-relay connection,
  NIP-42 AUTH, exact request correlation, `auth_url`, `switch_relays`, distinct
  communication/user keys, NIP-44 crypto, and frozen-event validation. Missing
  capabilities remain durable `AwaitingCapability`; a real redb close/reopen
  proof reconnects a bunker, promotes the exact frozen event, publishes, and
  receives a relay ACK. Swift projects Primal discovery and one-click launch;
  Kotlin/JVM projects package-filtered Android discovery and an exact
  URI/package handoff contract. Connections own scoped registrations, so a
  stale session cannot detach its replacement, and close/drop deterministically
  finishes only that session. An explicitly insecure SDK-owned plaintext file
  checkpoint now provides opt-in personal/development autologin (#197), while
  remaining distinct from the secure-provider contract. Still open under
  #47/#51: explicit per-write identity override, standard Keychain/Keystore
  providers and automatic secure-vault restore, NIP-55 execution/Android AAR
  integration, and permanent signer connection/correlation counters in engine
  diagnostics.
  The sign-only operation now projects across Rust, FFI, Swift, and Kotlin:
  it binds an immutable request to the active registered signer, validates the
  exact returned event, remains bounded/cancellable, and creates no
  store/outbox/publication residue. NIP-07 origin prompts and browser
  networking remain host policy rather than engine behavior.
- **Protocol-module composition is unbuilt.** The existing ownership design
  incorrectly makes kind ownership gate all route authority. Modules must claim
  only exact NIP-defined schemas while typed contextual operations may add their
  own tags and route facts to immutable foreign-owned drafts. No kind:1-first
  core catalog is part of the target.
- **~~Selector-projected values lost their only routable lane~~ CLOSED
  (#11).** `Tag(e/a/p)` now retains a valid tag relay hint or falls back to
  the source row's observed-relay provenance; `AddressCoord` retains source
  provenance. Typed evidence survives nested Derived/SetOp evaluation, is
  gated by discovered-relay admission, and reaches both public ids-only atoms
  and author outbox candidate solving. Duplicate source observations replace
  the live atom with enlarged evidence, identical inner demand remains globally
  refcount-shared across different selectors, and projected singleton ids are
  widen-only packed into wire filters capped at 256 ids. Sliding recent-window
  semantics remain separate; explicit NIP-01 inner limits are covered by #187.
- **The optional content substrate and first SwiftUI family are built; the
  multi-platform/open-code ecosystem remains open (#75).** `nmp-content`,
  governed UniFFI values, and Swift/Kotlin content
  clients now provide source-ranged plaintext/Markdown semantics, normalized
  NIP-19 references, kind:0/NIP-23 values, ordinary-demand acquisition,
  deduplicated claim/release, cycle/depth/target bounds, scoped evidence, and
  raw-event fallback. `NMPUI` adds fallback-safe Avatar/Name primitives, an
  arbitrary-native-view content flow, immutable local renderer sets, three
  mention treatments, generic event chrome, genuinely distinct portrait and
  Medium-style article cards, three user-card layouts, and three reaction
  interaction families. NIP-02 is now the first component whose protocol
  resource/action also ships: `NMPFollowing` projects canonical kind:3 state,
  `NMPEngine.follow`/`unfollow` own source-evidenced tag-preserving guarded
  replacement, and `NMPFollowButton` only renders and forwards the tap.
  Network-free scripted sessions make loading, shortfall, cycle, unknown-kind,
  and custom-document previews deterministic without an engine. The native iOS
  Gallery consumes those exact components against real data and configures only
  the two indexers; separate States and 72-row Stress tabs exercise Dynamic
  Type, RTL, reduced motion, dark appearance, long Markdown, and
  visible-reference claim release. Live Swift tests prove a profile mention and
  a relay-less NIP-23 `naddr` using only the two configured indexers and normal
  NMP outbox discovery. A real loopback parity proof drives follow, duplicate
  no-op, unrelated-contact preservation, and unfollow through direct Rust and
  the iOS FFI surface. Controlled relay identity/list primitives now ship in
  SwiftUI and a narrow optional desktop-JVM Compose subproject (#198). Both
  render caller-supplied one-shot NIP-11 state and query-scoped `SourceStatus`;
  they own no engine, HTTP, polling, cache, timers, or image loading. The
  Compose proof is not broad content/session parity and does not qualify an
  Android AAR. The conflict-honest `nmp-ui` source registry/CLI is now built
  (#165 via PR #475): `list`/`view`/`add`/`diff`/`update`, exact app-owned
  dependency closures, lock/merge-base hashes, three-way conflict evidence,
  and a SampleApp prove the adoption/update contract for its current two
  installable SwiftUI compositions. That is not a broad template catalog.
  Still unbuilt at this checkpoint: broad Compose UI parity and a Compose
  Gallery, broader registry/template breadth, NIP-25 live reaction resources/
  write intents (#155), and broader product/photo/highlight/media component
  families. The ordinary follow action also deliberately refuses
  first-contact-list creation; that requires a separately named policy/action
  before it can ship. See `docs/design/ui-components-strategy.md` and issue
  #75.
- **Boundedness is only partial.** Swift newest-frame buffering, indexed queries,
  and router caps exist, but graph, derived-set, wire, relay, result, receipt,
  ingestion, and scheduler bounds do not yet share an explicit shortfall
  contract. Silent first-N behavior is forbidden.
- **NIP-11 cache is process-local.** The engine now owns bounded one-shot
  acquisition, per-relay single-flight, finite typed waiter admission, HTTP
  validators/freshness directives, typed advisory limitation claims, raw JSON,
  stale-on-error, explicit refresh, and least-recently-used retention at a
  strict 256-document bound. Refreshing last-good documents count toward that
  bound and remain available for stale-on-error; if all 256 are refreshing, a
  257th fresh result is delivered but not retained. Fetches consume the shared
  zero-queue native-task ceiling and use cancellable Hickory DNS plus HTTP under
  one three-second deadline; engine shutdown closes every waiter and joins the
  task even when app handles survive. Relay URL credentials are rejected before
  request construction so reqwest cannot turn them into HTTP Basic
  `Authorization`; `RelayUrl` normalizes an empty userinfo marker to the same
  credential-free typed URL. Every redirect is refused before
  its target is contacted. Capability evidence retained by the reducer is
  limited to relays in the current read plan, pruned when that plan changes,
  and its diagnostic freshness is re-derived from the engine clock and the
  cited document's deadline rather than frozen at acquisition time. The cache
  is deliberately in memory for this first contract; a cold process does not
  reuse the prior process's relay document.
  Runtime connection/AUTH state also remains separate: NIP-11 acquisition does
  not invent a polling stream or claim that HTTP metadata is link state.
  Optional relay UI preserves stale last-good content with freshness and
  last-error evidence, represents no-snapshot failure as unavailable, and
  displays only caller-supplied query-scoped runtime status. Advertised icon
  text is exposed without dereferencing it; applications apply their own media
  policy and pass a SwiftUI `Image` or Compose `Painter`. The
  Swift wrapper tests run on the macOS host and the generated XCFramework's
  simulator slices compile, but an iOS Simulator runtime test target is not yet
  present ([#465](https://github.com/pablof7z/nmp/issues/465)). The Kotlin
  package remains a desktop-JVM projection; this work does not add or qualify
  an Android AAR. **The hidden cache/flight/waiter copy amplification is closed
  (#467).** One immutable payload owns the parsed document (including
  structured maps), exact raw JSON, and revision; cache entries, refreshing
  workers, 304/stale metadata versions, and all waiters share that payload.
  Runtime capability projection extracts only its compact evidence and retains
  no raw body. Under the default 12-task admission ceiling, the exact hidden
  raw-body envelope is 256 cached bodies plus at most 12 active
  response bodies: `268 * 256 KiB = 70,254,592 bytes` (67 MiB). A fully
  admitted 12-flight/64-waiter matrix therefore carries 768 shared snapshot
  pointers (6,144 bytes on the qualified 64-bit target), not 768 additional
  bodies. This is deliberately a raw-body bound, not a total-RSS claim: parsed
  values, relay URLs, HTTP metadata, maps, allocator/container overhead, and
  task/channel scaffolding are additional costs outside that number. The
  supported Rust facade still returns its existing ordinary owned value and
  UniFFI/native records remain owned; an application that concurrently
  materializes and retains 64 results owns those 64 copies by contract. A
  facade falsifier proves dropping them leaves exactly the one cached engine
  payload rather than a hidden waiter/result shadow cache.
- **~~Destructive trust-domain reset is missing as a defined contract~~ CLOSED
  (#232); in-process live-store deletion is structurally refused (#489).**
  `Engine::reset_persistent_store`, the UniFFI operation, and the Swift/Kotlin
  `NMPEngine.resetPersistentStore` projections idempotently remove one closed
  canonical store. `RedbStore::open` owns the guard at the lowest governed
  store layer: one mutex spans pre-open target resolution, create/open,
  post-open canonical target resolution, and registration, and the RAII
  registration moves with the store through facade, `from_parts`, or raw
  `EngineThread` ownership. Reset holds the same mutex through target
  resolution, live check, and deletion and returns typed
  `StoreStillOpen { path }` without touching a live in-process store. Existing
  and dangling final symlink paths resolve to the store target; reset never
  unlinks the alias inode. Reset clears cached events, pending writes, receipts,
  coverage/evidence, and related persisted state. Separately configured
  platform account checkpoints remain outside the store path and untouched.
  **Cross-process exclusion remains a gap:** no advisory/sidecar lock yet
  prevents another process from resetting a store that is live elsewhere, and
  arbitrary external symlink retargeting is not an in-process guarantee, so
  callers must coordinate that deployment boundary explicitly.
- **Public syntax remains provisional; its promotion protocol is now enforced.**
  Pinned snapshots cover the canonical `nmp` Rust facade and the
  language-independent UniFFI proc-macro component metadata. CI requires exact
  regeneration and a schema-complete append to `docs/surface-change-log.md`
  whenever either baseline moves, and rejects historical log edits/deletions.
  Hand-written Swift/Kotlin public wrapper paths and their consumer-visible
  package/build/settings manifests are governed directly even when generated
  snapshots do not move. The component extractor is a separately locked,
  base-trusted tool outside the product workspace. The trusted checker/workflow
  is loaded from the PR base so a proposed head cannot replace its judge. This first
  introduction is necessarily a manually reviewed bootstrap because no such
  default-branch workflow exists yet; after merge, enabling its required status
  is repository-settings issue #81.
  The Rust baseline also resolves definitions of dependency-owned types
  explicitly re-exported by `nmp` (#89), recursively including dependency-owned
  types reachable through variants, fields, aliases, and signatures, so those
  shapes—and public inherent constructors/methods on the explicitly re-exported
  root definitions—cannot move behind an unchanged opaque `pub use`; unrelated
  mechanism APIs, nested helper impls, and trait/auto/blanket impls remain
  outside the supported snapshot.

## Load-bearing for M5 (the falsifier app) — must close before M5 claims pass

- **~~`RelayDirectory` has no reactive update path~~ CLOSED (self-bootstrapping outbox).** `nmp_router::LiveDirectory` is a live, updatable `RelayDirectory` (write relays start empty, fed at runtime via `RelayDirectory::ingest_write_relays`); `nmp_engine::core::EngineCore::sync_discovery` watches active content demand for authors whose write relays are still unknown and opens an internal kind:10002 discovery subscription against the configured indexers for exactly them (reusing the ordinary resolver subscribe/unsubscribe machinery, not a parallel subscription system) -- when that kind:10002 lands, the winning event is re-read from the store and fed into the directory, and the very same recompile re-routes that author's content atoms to their real write relay. `nmp-demo`'s two-phase `bootstrap.rs`/`BootstrapDirectory` are deleted; the CLI now configures only two indexer relays and gets real notes with the engine doing discovery. `nmp-ffi`'s `NmpEngineConfig`/Swift `NMPConfig` lost the `write_relays`/`writeRelays` field for the same reason -- an app supplies indexers only. Headless proof: `nmp-engine/tests/self_bootstrap_outbox.rs`. Live proof: `nmp-demo` against real relays, and `Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`.

- **~~Publish payload is unsigned-only across FFI (M4)~~ CLOSED (#32); verify placement updated (#52 Unit A0/Unit B).** `FfiWritePayload` now has `Unsigned`/`Signed` variants (mirroring `nmp::WritePayload`); a caller holding an already-signed event (external signer / NIP-46 bunker / verbatim republish) submits `.Signed` and the engine publishes it verbatim -- no re-sign, no tag mutation, no id recomputation. The verify moved OFF the FFI boundary (#52 Unit B): `convert::signed_event_from_ffi` only PARSES the reconstructed event's fields now (typed `FfiError::InvalidSignature` for a malformed sig hex, `InvalidEventId`/`InvalidPublicKey`/`InvalidTag` for other malformed fields) -- there is no `FfiError::InvalidSignedEvent` anymore. `nostr::Event::verify` instead runs at `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary (Unit A0/#56), so the guarantee holds for every entry point (this facade, direct-Rust, `from_parts`), not only FFI. A tampered `Signed` event parses fine at the FFI boundary and is rejected downstream, surfacing as `WriteStatus::Failed` -- the first and only status -- on the receipt stream, never a synchronous `FfiError`. Swift/Kotlin `WritePayload`/`WriteStatus` mirror both cases. Falsifier: `crates/nmp/src/engine.rs`'s `tampered_signed_publish_fails_closed_with_no_accepted` (direct-Rust), `crates/nmp-ffi/src/convert.rs`'s `ffi_publishes_presigned_event_verbatim`/`ffi_presigned_never_resigned`/`tampered_signed_event_still_parses_verify_moved_downstream`/`ffi_rejects_signed_event_with_unparseable_signature`, `crates/nmp-ffi/src/facade.rs`'s `ffi_tampered_signed_publish_fails_closed_on_receipt_stream`, plus `Packages/NMP/Tests/NMPTests/FilterBuilderTests.swift`'s `testSignedWriteIntentConversion`.

- **~~kind:10002 discovery over-fetch (7112 events for a 39-author set)~~ CLOSED (churn fix).** The M5 dogfooding session's diagnostics screen showed `wire_sub_count: 1` / `authors_served: 39` / a SINGLE, correctly-scoped `{"authors":[...39...],"kinds":[10002]}` filter against `purplepag.es` -- yet `events kind:10,002 = 7112` had been received. Root-caused (NOT the "unscoped/wildcard filter" theory the wire evidence had already ruled out): `nmp_engine::core::EngineCore::sync_discovery` tore down and reopened the internal kind:10002 discovery subscription as a fresh overwriting `Req` every single time an author's write relays resolved (each resolution shrank the filter's `authors` set by exactly one). To a NIP-01-compliant relay, an overwriting `Req` on an already-open sub-id is indistinguishable from a brand-new subscription: it replies with a full EOSE replay of every currently-matching stored event. Resolving N authors one at a time this way sums to a triangular N+(N-1)+...+1 redelivery, not O(N) -- confirmed exactly by a headless falsifier (`nmp-engine/tests/discovery_churn.rs`): 39 authors resolving one-by-one pre-fix produced 39 separate `Req` ops and 819 total author-resends (~triangular ceiling 780); post-fix, 1 `Req` op and 40 resends. **Fix:** `sync_discovery` is now widen-only -- a newly-needed author still widens the subscription (unchanged), but an author leaving `needed` no longer tears anything down; it's simply left in the filter (widen-safe: a wider author set only ever matches MORE, never fewer, the same proof obligation `nmp_router::coalesce`'s `AuthorUnion` rule already carries) until `needed` goes fully empty, at which point the subscription actually closes. Live-relay re-verification (`nmp-demo` against `purplepag.es` + `relay.primal.net`): 196+45=241 total kind:10002 events for a 193-author resolved set (~1.2x, not ~182x). Also fixed as part of the same investigation: `nmp_router::facts::LiveDirectory::ingest_write_relays` removed an author's directory entry entirely when their kind:10002 declared zero write relays, instead of recording "known, zero relays" as the trait's own contract requires (`RelayDirectory::ingest_write_relays`'s doc) -- and `nmp_router::facts::DiscoveryKinds`'s default now covers kind:0/3 plus the WHOLE NIP-01 replaceable range (10000..=19999), not just the four kinds NMP happened to read (owner-affirmed semantics).

- **Unbounded historical replay can peg the main thread (M5 dogfooding finding), bound across two halves (#17).** `apps/Falsifier` (the M5 SwiftUI app) reproducibly saturates a simulator's main thread at ~97-98% CPU for 1-2 minutes, twice: (1) whenever a query without a `limit` (e.g. the app's `FeedFilters.followsRelayLists()`, `kinds:[10002]`) is freshly `observe`d, and (2) whenever `observeDiagnostics()` is first iterated. `sample` on the running process shows sustained top-of-stack time in `nmp_store::redb_store::RedbStore::EventStore::query` plus `serde_json`/schnorr-signature JSON parsing, not idle waiting -- real, repeated work, not a hang (it does eventually finish and CPU returns to 0%).
  - **Swift-delivery half CLOSED.** `NMPQuery`/`NMPDiagnostics` used to re-deliver the full accumulated snapshot on every single delta (no batching/coalescing), so an ordinary app iterating `for await batch in query` with ordinary SwiftUI `@State` writes got many consecutive full re-renders, starving the run loop. **Fix:** `Packages/NMP/Sources/NMP/FrameCoalescer.swift` -- `RowBridge`/`DiagnosticsBridge` now coalesce delivery to at most one snapshot per ~16ms (~60Hz) window, always the LATEST accumulated state (no delta is ever dropped from the final state, only intermediate *deliveries* are), plus `.bufferingNewest(1)` on both `AsyncStream`s so a consumer slower than the coalescing cadence still can't accumulate a growing backlog. Live-relay-verified (`Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`, real replay against `purplepag.es`/`relay.primal.net`) plus a dedicated unit falsifier (`FrameCoalescerTests.swift`) proving a 200-push tight-loop burst collapses into a handful of deliveries with the final delivered value exactly equal to the last pushed value.
  - **Rust query-cost half CLOSED (#38); per-event refresh cost now bounded — on-device re-verification pending.** `nmp-store`'s `RedbStore::query` used to decode every row's JSON with no index narrowing (the dominant `sample` cost). **Fix (#38):** two persistent redb secondary indexes (`BY_AUTHOR`/`BY_KIND`) maintained in lockstep through the one centralized `remove_row_in_txn`/insert path (so they cannot drift across supersession/kind:5/expiry/gc); `query` now does bounded index range-scans for id/author/kind/address filters and only JSON-decodes the narrowed candidate set (falsifier: an author-filtered query over 1 target + 200 noise rows decodes exactly 1). The other named cost — `crates/nmp-engine/src/core/mod.rs` refreshing all handles after every ingested event — is unchanged, but each refresh is now a *cheap indexed* query rather than a full-table scan, so the O(events × handles) blow-up is bounded. **Honest status:** the root cause is fixed and the Swift-delivery half caps re-render frequency, but the ~97% CPU jank has NOT been re-measured on device with all three fixes (Swift coalescing + Rust index + churn) live — verify the running result on the Falsifier before declaring the M5 jank gone. Screenshots: `docs/screenshots/m5-06-diagnostics-loading-jank.jpg`, `m5-07-diagnostics-steady-state.jpg`.
  - **NIP-29 tag/limit amplification CLOSED at the store boundary (#142); device room-open verification pending.** `BY_AUTHOR`/`BY_KIND` still left `kind:9 & #h=<group> & limit:200` decoding every cached kind:9 event across every room, and the complete-set `EventStore::query` door cannot safely honor `limit` because reactive recompute and negentropy require its full answer (#124/#139). **Fix:** redb now maintains a generic NIP-01 single-letter tag index keyed by tag/value/`created_at`/event-id in the same transaction as every canonical mutation and rebuilds it crash-atomically on legacy reopen. A separate `query_newest` door reverse-scans one ordered tag bucket and stops after N accepted rows; handle projection uses that bounded door per root atom, then preserves the authoritative final merged global top-N. Real persisted corpus: 1,062 kind:9 rows, busiest `#h` room 557 rows, `limit:200`; 50-iteration release mean fell from 5.150 ms to 0.784 ms (6.57x), and full-event JSON/crypto reconstruction fell from 1,062 candidates to 200. This proves the store cost drop, not yet the end-to-end device UX; the remaining binary-record/planner/batch work is tracked under #148.
  - **Nested-JSON canonical event rows CLOSED (#150), then split immutable-note storage CLOSED (#162).** Canonical v3 rows are endian-defined binary values addressed by monotonic `u64` surrogate keys: immutable id/pubkey/signature/time/kind/tags/content bytes live in `EVENTS`, raw 32-byte ids resolve through `EVENT_IDS`, and relay/local provenance lives in a separate binary metadata sidecar. Every ordered/address/expiry index stores the surrogate key; canonical lowercase 64-hex tag values occupy 32 raw bytes in the tag index. Query predicates borrow fixed fields and tag/content slices from the redb value guard, so rejected candidates never construct `nostr::Event`, parse hex, or reconstruct secp types. An exact equal-or-earlier relay replay reads only the metadata sidecar and performs no write at all; signature adoption rewrites the immutable note only when the signed event actually changes. The v3 change is intentionally schema-breaking: opening a file containing a legacy event epoch now fails before any v3 table is created, so old outbox/coverage facts can never run beside an empty v3 event store. Differential matching tests pin equivalence with `nostr::Filter::match_event`, and a raw referential-integrity audit covers supersession, duplicate provenance, kind:5, NIP-40, GC, compensation, and every crash seam. On the 1,114-event real corpus (1,062 kind:9, busiest room 557), the bounded room query measured 0.260 ms versus the original 5.150 ms; a 1,114-event exact replay measured 6.102 ms versus 24.98 ms before the split, and 20 exact passes left the 4,214,784-byte redb file unchanged. The surrogate is a lookup/CPU win, not a claimed size win: v3 logical stored bytes were 1,486,162 versus v2's 1,474,770 (+11,392, 0.77%); its five query indexes were 475,137 versus 465,940 (+9,197, 1.97%) because exact tie ordering still retains the full id while each row gains an eight-byte value. The checked-in `storage_stats` example reproduces physical and per-table accounting across both schemas. Many *distinct* relays still grow and rewrite the variable-length sidecar: relay-url interning/fixed-width observations remain open under #148. These remain store microbenchmarks; end-to-end device room-open verification is still pending.
  - **Relay URL interning and fixed-width per-event observations CLOSED (#167).** This supersedes the final “remain open” sentence in the historical #162/v3 bullet above. Canonical v4 stores optional local intent state in a dedicated `NMPL` value and each relay observation as one fixed 12-byte `(event_key:u64, relay_key:u32)` key plus an eight-byte latest timestamp. Relay URLs are interned once behind bijective forward/reverse tables with exact refcounts; removing the last observation reclaims the URL, while monotonic relay keys are never reused. Exact/equal replay point-checks one observation and writes nothing; a later timestamp replaces one eight-byte value; a new relay adds one fixed row without rewriting event or local bytes. A transaction accumulates effective refcounts in memory and flushes the hot row once per distinct relay, including bulk insert, expiry, GC, supersession, and compensation. Query materialization joins observations only after borrowed event filtering and caches each parsed relay URL once per query. Every observation/event/relay/refcount relation is included in the raw exact-set integrity audit and a process-abort seam proves dictionary, observation, refcount, event, indexes, and outbox remain one atomic fact. The checked-in `ingest_bench` now reproduces a 1/20/100-relay matrix from a real current store, including busiest-room newest-200, complete and reopen-first queries, exact-replay growth, and logical/physical bytes. A three-run matrix on the 1,114-event corpus (1,062 kind:9; busiest room 557) measured 0.296/0.700/2.678 ms for room newest-200, 1.691/4.640/18.368 ms for complete queries, 4.943/5.969/5.998 ms for exact replay with zero file growth, and 1,437,260/1,862,424/3,652,664 logical bytes. At 100 relays the physical file was 16,809,984 bytes. For historical scale, the earlier v3 101-relay run measured 6.571 ms room, 36.008 ms complete, 30.523 ms per new-relay pass, 6,168,304 logical bytes, and 29,700,096 physical bytes. Public `Provenance` construction necessarily remains proportional to returned observations; the avoidable URL reparsing, variable-sidecar COW, and repeated hot-refcount writes are closed. Device room-open verification remains pending.
  - **Ordered one-best-index query planning CLOSED (#149); device verification pending.** The author and kind indexes are now binary `(field, created_at, !event-id)` rows, joined by global-created-at and author+kind indexes with the same suffix. `query_newest` chooses one best index (author+kind, author, the smallest tag value set, kind, then global time), reverse-scans newest-first, and applies every remaining filter to the borrowed binary event. Single ranges stop directly at the requested visible limit; OR values are exact k-way merges with id deduplication and the canonical `created_at DESC, id ASC` tie-break. All index mutations remain inside the same crash-atomic transaction as events, coverage, and outbox state; legacy indexes rebuild atomically from canonical rows before their schema marker is published. Tests prove kind/global scans materialize exactly N rows, multi-tag OR order is exact, rejected candidates stay borrowed, and legacy reopen backfills the new indexes. On the real 1,062-row corpus, 100-iteration release means were 0.373 ms for the busiest room, 0.299 ms for kind:9, and 0.317 ms for the global newest 200. The original room baseline was 5.150 ms; end-to-end device room-open remains to be re-measured.
  - **Cardinality-aware complete/bounded planning and streaming execution CLOSED (#169); device verification pending.** The shape-priority planner and complete-query candidate `HashSet` unions/intersections are gone. redb now persists exact live-row counts for global, author, kind, author+kind, and every single-letter tag/value prefix. One transaction-owned index bundle accumulates checked deltas in memory and flushes each touched prefix once in the same crash-atomic commit as canonical events, indexes, coverage, relay observations, and outbox effects; duplicate tags count one physical row, zero rows disappear, and an independently versioned sidecar rebuilds atomically by counting ordered index keys without dereferencing canonical event values before publishing its marker. Both complete and bounded reads generate every applicable bounded-fan-out plan, choose the smallest persisted physical bucket, retain one reverse redb iterator per OR prefix, heap-merge in canonical `created_at DESC, id ASC` order, and apply only unmatched predicates to the borrowed binary view. Multi-value overlap deduplicates the immediately repeated surrogate without an unbounded candidate set; bounded reads stop without advancing or dereferencing the next candidate after the visible limit, and author×kind Cartesian planning is capped before allocation. Exact instrumentation distinguishes index entries, borrowed event-value dereferences, and owned materializations. Tests include 100 deterministic mixed filters differential against `MemoryStore`, empty-set/reversed-window semantics, selected-tag masking, overlapping tag OR, bounded composite fan-out, exact raw cardinality audit across every governed mutation/crash test, and missing-epoch rebuild. The checked `query_bench` imports JSONL directly and measures elapsed time and allocation operations for complete/bounded global, kind, author, author+kind, `#h`, `#h+#p`, 43 populated real-corpus authors, rejected-heavy search, and reopen-first reads. On the preserved 1,114-event corpus (557-row busiest `#h`, 140-row matching `#p`), the corrected 50-iteration release comparison measured 0.209 ms versus 0.348 ms before #169 for bounded `#h+#p`, 0.213 ms versus 0.366 ms for complete `#h+#p`, and 0.196 ms versus 0.298 ms for complete author+kind. Complete kind remained neutral at 1.559 ms versus 1.572 ms, as expected when nearly the entire 1,062-row bucket is the answer; rejected-heavy search fell from 0.188 ms to 0.005 ms. Counting-allocator comparisons show the removed candidate sets without overstating them: complete `#h+#p` used 2,799 allocation operations versus 2,863 and complete global used 19,934 versus 20,217, while owned result materialization remains the dominant allocator cost. A warmed real-corpus write matrix measured the crash-atomic 1,114-event batch at 20.975 ms versus 20.814 ms before #169 (+0.8%, within run noise); smaller batch sizes were likewise neutral rather than paying one redb write per index row. End-to-end device room-open remains pending.
  - **Bounded interior `Derived` projections CLOSED (#187); device verification pending.** A `Derived` binding kept its inner NIP-01 `limit` in the descriptor and wire filter but used complete-set `EventStore::query` for local construction and recompute, silently turning “authors of the newest 200 matches” into a full-history materialization. The resolver now selects explicit limits through `query_newest` before applying its closed selector; unlimited derived nodes and negentropy retain the complete query door unchanged. A generic falsifier proves an older row outside top-N cannot affect the derived set, a newer row evicts exactly the old floor, and kind:5 retraction pulls the next-newest row back in. On the #186 million-row fixture, the real resolver subscription over a 59,915-row hot bucket fell from 3,786.191 ms p50 to 0.730 ms p50 (1.406 ms p95) while producing the same 33 demand atoms. This is generic resolver semantics, not NIP-29-specific storage logic; #176 still owns physical-device closure.
  - **Portable packed tag/string arenas CLOSED (#170); device verification pending.** Immutable event codec v4 keeps the 158-byte fixed header, then stores cumulative tag ends, one four-byte atom descriptor per element, a dense arena, and directly addressable content. Descriptors inline zero-to-three-byte UTF-8, point to shortest-form LEB-length UTF-8 cells, or point to raw 32-byte canonical lowercase-hex identities; borrowed tag iteration returns text/raw views, and each query prepares raw wanted values once for binary search, so rejected candidates neither allocate nor hex-encode. Full validation rejects overflow, gaps, overlap, unused arena tails, non-zero reserved/padding bits, overlong LEB, invalid UTF-8, empty tags, representation aliases, truncation, and trailing bytes. The encoder makes two classification passes but allocates only the final value; materialization alone recreates exact lowercase hex strings for returned rows. Unchanged local/provenance sidecars retain codec v3, composite displaced rows move to v4, and the whole crash-atomic store bundle moves to rejecting epoch v5: any v4 event/displaced table aborts open before one v5 table is created, with no compatibility path. On the preserved 1,114-event corpus (2,543 tags, 5,085 atoms; 2,535 inline and 1,348 raw32), immutable values are 881,779→837,122 bytes (-5.064%) and the events table is 890,691→846,034 stored bytes (-5.014%). Five identical paired event-only redb builds measured 2,670,592→2,584,576 compacted bytes (-3.221%); full-store compacted file size is deliberately not claimed because redb 4.1 compaction is bimodal under layout entropy. A tag-heavy NIP-29 falsifier is 1,487→959 bytes (-35.51%). Alternating same-session real imports measured median 34.97 ms for v3 versus 34.01 ms for v4, while the codec itself encoded all 1,114 events in 0.187 ms; paired room/member/global queries remained within run noise and exact results were unchanged. End-to-end device room-open remains pending.
  - **Parallel verification + single-writer batch ingest CLOSED (#151); one table bundle per governed batch CLOSED (#164); device verification pending.** Transport workers still feed one pool-global verified-id/signature cache, but the translator now drains bursts of up to 128 frames and runs first-seen schnorr verification concurrently on native targets (the same code has a sequential wasm fallback); known ids remain cheap signature comparisons. The runtime preserves frame order while coalescing queued frames into one resolver call. `EventStore::insert_batch` runs the exact governed insert state machine in input order inside one redb write transaction and commits once, including event rows, every ordered/tag/expiry/address index, kind:5 effects, provenance adoption, and outbox satisfaction; any persistence error aborts the whole batch. The v3 writer opens that transaction's canonical/index/outbox tables once and reuses the bundle for every event rather than reopening every table per row. The resolver reacts once to the combined insert/remove set and the engine recompiles/refreshes once per burst. Both backends share contract tests for input-order supersession/provenance equivalence. On the same 1,114-event corpus, one current-schema release import measured 22.575 ms versus the prior 29.8 ms all-event batch; batch size 128 measured 76.3 ms versus 103.2 ms. The checked-in `ingest_bench` also measures exact duplicate replay and physical file growth. This isolated the store transaction cost; the persistent-worker and end-to-end measurements it originally left open are superseded by #168 below.
- **Parse-once typed relay ingest and persistent bounded verification CLOSED (#168); device verification pending.** The websocket boundary now parses each text frame exactly once into a typed `RelayMessage`; EVENT payloads move immediately into `Arc<Event>`, and verifier workers plus the engine share that allocation until the engine unwraps it for binary persistence, so the old `Value -> event JSON -> Event -> original frame parse` chain, production first-seen deep clone, and transport's direct `serde_json` dependency are gone. Native verification uses a fixed persistent worker set with one reusable secp context and one bounded queue per worker; wasm keeps the same ordered API with deterministic sequential verification. Crypto runs outside `PoolInner`; every payload recomputes its event id exactly once before identical unknown `(id, signature)` pairs may share signature work, preventing same-batch or cached-id admission of mutated content/tags/time/kind. Generation planning applies same-batch reconnect transitions in FIFO order and the real state is rechecked after verification, so close/reopen cannot admit stale work; cache capacity is explicit. A failed verifier lane rejects its affected task, surfaces engine diagnostics without falsely incrementing relay-misbehavior, and is replaced before future batches. Worker-to-translator and pool-to-engine queues are bounded; engine transactions are independently capped at 128 frames; an applied acknowledgement prevents another relay batch from entering the engine until resolver/store effects finish. Shutdown disconnects an event-driven cancellation channel, releasing a bridge waiting for ack and any blocked bounded producer without polling; immediate durable-send failures resolve locally rather than re-entering the engine's own queue. Tests pin mixed-frame order including EVENT-before-EOSE, same-batch reconnect, stale generations, mutated/invalid/mismatched signatures, cache eviction, worker replacement, transaction caps, backpressure cancellation, shutdown behavior, and real relay reconnects. A checked release rerun over the preserved 1,114-event corpus measured 2.307 ms for single relay-message parse plus shared allocation, 6.752 ms for the honest full first-seen path (event-id recomputation plus persistent-worker signature verification), and 3.272 ms for known-redelivery event-id recomputation plus signature checks. The typed resolver-to-governed-redb harness measured 40.373 ms on the hardened tree versus 46.140 ms on the prior PR head in the same session; an earlier lower-I/O run measured 18.712 ms, exposing filesystem variance but no regression from this hardening. The direct wasm compile remains blocked before NMP code by the workspace's existing `getrandom`, `ring`, and `secp256k1-sys` target configuration; the source keeps a thread/channel-free wasm branch. End-to-end device room-open verification remains pending.
- **Transport/verifier and native task OS-thread ownership CLOSED (#442, #446).** Every engine owns exactly two persistent native verifier workers (one on wasm's sequential path), one transport translator, one relay-retirement reaper, and one native-task reaper. `max_relays` bounds demanded live relay workers plus an equal charged retirement allowance; `max_native_tasks` (default 12) separately bounds immediately-running observer/action/signer tasks with no queue. A native bridge OS thread is registered before its query, receipt, reattachment, or action transfers ownership. Logical saturation is the distinct typed `ExecutorSaturated { component, capacity }`; OS refusal remains `ThreadUnavailable`, and already-accepted signer work remains retryable without losing its durable obligation. Completed tasks and retired relay workers remain charged until their owned reapers join them. Shutdown actively signals producer/cancellation doors, invalidates forgotten reservations, avoids callback self-join, and exposes an event-driven exact-zero census without polling or timeout inference. Rust/FFI/Swift/Kotlin falsifiers cover cap-sized saturation, one terminal callback, cancellation, repeated lifecycle baseline, callback-initiated shutdown, and injected reaper/task spawn refusal. The full ordinary native envelope and the separately bounded NIP-46 session-pool qualification are recorded in `docs/design/native-task-executor.md`.

## Real but non-blocking for the falsifier (feeds, not DMs)

- **~~DM inbox routing incorrect (M3-D)~~ CLOSED (#19).** `WriteRouting::ToInboxes` used to fall back to the union of recipients' *write*+extra relays because `RelayDirectory` had no read/inbox accessor. **Fix (#19):** `RelayDirectory` grew `read_relays` (lane `Nip65Read`) + `ingest_read_relays`; `LiveDirectory` stores both read- and write-marked kind:10002 entries from the same winning event (`parse_nip65_read_relays`, NIP-65 unmarked = both); and `EngineCore::resolve_routes`' `ToInboxes` branch now consumes `read_relays` ONLY — a recipient with no known inbox relay (unknown or write-only) fails the whole intent CLOSED with a typed `Failed` before any `PublishEvent`, never falling back to write relays. Falsifiers: `core_headless.rs` `to_inboxes_*` (read-only routes, write-only + unknown fail closed) and `core::nip65_read_write_split_tests` (unmarked=both, read/write-marker split, one-winner-both-sets).

- **Decrypt-result feedback path missing (M3-C, plan §8 item 2).** `Effect::RequestDecrypt` is an explicit no-op; there is no `EngineMsg` to feed a decrypt result back into ingest. Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12 encrypted-content path). Deferred with E/negentropy still open; not on the falsifier's feed path.

- **Reconnect loses negentropy-first temporarily (M3-E).** On reconnect, subs previously routed negentropy-first are replayed as plain REQ (safe, correct — just less efficient) until the next real demand change re-routes them. A "reroute negentropy-first on reconnect" refinement was deferred. Perf, not correctness.

- **~~No time driver for liveness/timeout sweeps (M3-E)~~ CLOSED (#39 via PR #42).** The engine loop's `cmd_rx.recv()` is now `recv_timeout(next_deadline − now)`, armed from `EngineCore::next_deadline()` (min over the store expiration index + neg-session liveness deadlines): zero new threads, wakes exactly at the next real deadline, blocks forever on `recv()` when none exist, re-arms every iteration (an ingest introducing an earlier deadline re-arms naturally — no interrupt machinery). NIP-40 expiry fires event-driven through the same driver. Review caught + fixed a ~1s 100% CPU busy-spin (the neg-liveness sweep threshold `> N` was misaligned with the armed deadline `started_at + N`; now `now >= started_at + N`, so the tick that fires the deadline also clears it); regression test (`neg_liveness_deadline_does_not_busy_spin`) hand-verified failing pre-fix at ~986ms CPU, passing post-fix. D8-clean, no polling.

## Design-level (validated from external feedback — see docs/reviews/2026-07-11-external-feedback-triage.md)

- **~~Supersession retraction blindness~~ LANDED (#195 via PR #227; #228 via PR #230).** The symmetric negative-delta lane described in `docs/design/retraction-and-negative-deltas.md` now carries exact inserted, removed, and provenance-growth facts from relay ingest, durable local acceptance, pre-signature compensation, and NIP-40 expiry through the resolver/engine boundary. Stable complete simple handles apply those committed facts without reopening their full result set; bounded handles retain exact top-N backfill, while derived, multi-root, Strict-cache, incomplete, demand-changing, evidence-changing, or otherwise unproven shapes conservatively keep the full-refresh oracle. Differential and counting-store falsifiers cover replaceable supersession, provisional kind:5 suppression/reveal, compensation restoration, expiry, duplicate/stale/refused no-ops, and mixed mutation sequences. The design document's optimistic-write details remain superseded: durable accepted rows use typed `Pending(intentId) | Signed(signature)` state, and only cancellation or terminal **pre-signature** failure retracts and compensates a displaced replaceable; relay rejection after signing changes receipt evidence only. `docs/design/durable-write-signing-and-retry.md` owns that correction. Permanent kind:5 tombstone retention is built under the owner decision recorded in #23; #176 still owns end-to-end physical-device room-open verification.

- **~~Four bounded correctness fixes from the external-feedback triage~~ LANDED (merge `9220f65`).** (1) Signature-verification gate at the network layer (`nmp-transport` frame seam) — kind-independent, verify-once per event id (redelivery string-compares the cached sig, no re-schnorr), invalid sig → drop + `RelayHealth::invalid_signature_count`; cache reads never re-verified. Makes ledger #5 honest. (2) FFI no longer panics on malformed `Literal` hex (typed error) and no longer silently drops malformed tags (`tags_from_ffi` returns `Result` — NMP can't sign a different event than the app composed). (3) `DescriptorHash`/`CoverageKey` widened FNV-64 → BLAKE3 256-bit (a network-controlled, durable-and-refcount key must be collision-resistant; a forged collision there would attach a watermark fact to the wrong filter). (4) `coalesce` never merges limited filters (relay-side truncation under-fetch), and a known-zero-write-relay author stops perpetual discovery.

## Persistence robustness

- **Fallible ingest/read store doors landed; two read peeks and FFI plumbing deferred (#122).** The six ingest/read `EventStore` doors (`insert`/`query`/`remove`/`expire_due`/`record_coverage`/`gc`) now return `Result<_, PersistenceError>` — `RedbStore` propagates real redb I/O errors instead of `.expect()`-panicking on every EVENT frame, and the engine degrades the local cache to read-only (a `DiagnosticsSnapshot.store_degraded` signal) instead of crashing the host app. Deliberately NOT done in that change, flagged here so nothing hides: (1) `EventStore::next_expiration` and `get_coverage` (small index/coverage read peeks) are still `.expect()`-on-I/O — a disk error there still panics; widening them ripples into the engine's deadline-arming hot path and was scoped out. (2) `store_degraded` is surfaced only on the Rust `DiagnosticsSnapshot`; it is NOT plumbed through `FfiDiagnosticsSnapshot`/Swift/Kotlin, so a native host cannot yet observe the read-only degrade. (3) The degrade policy is intentionally minimal (latch first error, skip the reactive step, emit a diagnostic, keep running) — there is no recovery/reopen path, no per-door policy, and no bounded-retry framework.

## Security hardening deferred

- **Secret zeroization and platform signer-provider boundary are not complete.** NIP-46 URI/session secrets use redacted debug output and zeroizing memory, and the durable event/outbox store persists only the expected pubkey plus an opaque identity reference. `LocalKeySigner` still holds `nostr::Keys` without the old repo's raw-bytes/zeroize hardening, and Swift/Kotlin do not yet ship standard secure-storage-backed providers that restore sessions automatically. Owner: security/signing workstream (#47).

## Process / tooling

- **Required-status branch protection is not configured (#81).** Ordinary CI
  and the trusted-base `surface-governance` workflow exist, but repository
  settings must require both `surface-governance` and the protected ordinary
  `surface-regeneration` check after the governance bootstrap merges.
