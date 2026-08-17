# Known gaps & deferred follow-ups

Honest running list of open problems and deliberately-deferred work, so nothing
hides. Each entry is a current unresolved item with its consequence and owning
open issue. Fixed items are deleted (git/history remembers them), not narrated.

## Capability & signing

- **No FFI-crossing door for an app-implemented signing capability.** A Secure
  Enclave or hardware-backed key reachable only from Swift/Kotlin has no way in.
  Whatever closes it must keep NMP the owner of when/what to sign, with the app's
  adapter merely interfacing to the hardware; the deleted app-supplied signer
  mailbox (#1290) inverted that ownership and was removed rather than kept.
- **NIP-42 deadlocks against any relay that challenges in response to a
  request (#1889).** An app declaring `NMPAccessContext.nip42` against strfry
  never exchanges a byte with it: NMP withholds a protected session's REQs
  until AUTH completes and only starts AUTH on an INBOUND `["AUTH", challenge]`
  frame, while strfry only challenges in response to a request it wants to
  gate. The query sits at `awaitingAuth(phase: .awaitingChallenge)` forever,
  the installed `NMPAuthPolicy` is never consulted, and `AuthDiagnostics`
  reports a placeholder row indistinguishable from a session that has merely
  just connected. Every green NIP-42 test either injects a synthetic challenge
  into the reducer or uses a relay hand-built to challenge unsolicited on
  connect, which is why nothing caught it. Canary C15 is the committed
  reproduction and is red until this closes.
- **AUTH-policy callback inversion still open (#783).**
- **Session storage is app-owned: NMP ships no plaintext checkpoint and no
  automatic Keychain/Keystore session store.** Transactional app-owned session
  storage tracked in #1398.
- **Permanent signer connection/correlation counters are not in engine
  diagnostics.** NIP-55 execution and Android AAR integration also remain open.

## Routing & limits

- **Boundedness is only partial.** Swift newest-frame buffering, indexed
  queries, router caps, and the expandable observation window are bounded, but
  graph, derived-set, wire, relay, ordinary-result, receipt, ingestion, and
  scheduler bounds do not share an explicit shortfall contract. Silent first-N
  behavior is forbidden (ledger #17).
- **A pending cancel hook that blocks forever blocks `EngineThread::join`.**
  The runtime's finite drain cancels every live policy/signer operation without
  polling and never blocks the engine thread on app code, but safe Rust cannot
  force-kill app code invoked synchronously by a pending operation's `Drop`
  cancel hook while the runtime is being joined. A well-behaved `AuthPolicy`/
  signer drains cleanly; the symmetric property already holds for the
  sign-event path.
- **NIP-29 `previous` tags remain omitted.** NMP cannot mint them until a
  host-scoped, group-scoped, author-aware live-window capability can do so
  without caller tuples, truncation, or transplantation.
- **Watching very many NIP-29 groups at once needs sharding the app does itself
  (#1233).** The group-records observation opens one branch per HOST, never per
  group, so the strain is the `#d` value set inside one relay filter, which a
  relay may refuse or silently truncate (NIP-01 sets no bound). NMP does not
  chunk behind the app's back and offers no threshold guidance because the
  answer is per-relay and undiscoverable.
- **Coordinate gate has three residual limits (#1630/#1631/#1668).** (1) A lane
  that asks and finds nothing outstanding still sends, and if the relay held a
  newer list the loss is terminal. (2) The coverage question is asked on the
  authenticated session only when AUTH completed; a relay serving an
  authenticated reader a different list than a public one can still be
  overwritten. (3) The 500-frame bound is fixed, not read from a relay's
  advertised NIP-11 `default_limit` (#744).
- **Decrypt path is absent end-to-end (M3-C).** Ingest never asks for a
  decryption and there is no `EngineMsg` that could carry a plaintext result back;
  the reducer emits no decrypt effect and the runtime has nothing to execute.
  Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12); #6 owns the
  async sign/encrypt/decrypt capability design that would supply both halves.

## Observation & delivery

- **The M5 on-device jank has not been re-measured with all fixes live.** The
  store/query/ingest/transport fixes (secondary indexes, bounded interior
  `Derived`, packed arenas, parse-once typed ingest, batch ingest, async
  pull-based observation handles, internal-admission elimination) are closed in
  code and falsified in headless tests, but the ~97% CPU jank has NOT been
  re-measured on a physical device with the Swift coalescing + Rust index +
  churn fixes all live. Verify the running result on the Canary before
  declaring the M5 jank gone. Many of these fixes also carry "device room-open
  verification pending" — the same on-device pass closes them.
- **Engine-lifetime memory grows linearly in DISTINCT filters observed, and
  closing the observation never releases it (#1846).** Canary C17 measured
  +291 B per additional distinct filter between 300 and 1200 cycles and
  +541 B between 1200 and 4000 — linear, no plateau, with `phys_footprint`
  rising 9.49 MB → 13.78 MB across 4000 closed observations. The identical
  loop with the SAME filter is flat (−0.5 B per additional cycle), so this
  is keyed on the filter being new, not on open/close overhead. Released in
  full by `shutdown()`. Whether it is an unbounded in-memory map or the
  store's page cache holding durable per-filter coverage rows cannot be
  determined through the public API — `DiagnosticsSnapshot` exposes no
  retained-bookkeeping or memory fact at all. C17's `distinct` phase is red
  until this is answered.
- **A cold start never reconciles over NIP-77; it always refetches (#1888).**
  `begin_neg_handoff` is only reachable when the relay already carries a
  behaviorally-minted probe verdict at the moment a request is placed, and the
  probe is asynchronous — a fresh engine sends its query's REQ as soon as the
  socket is up and learns the relay supports NIP-77 just too late to use it.
  Nothing re-plans the in-flight request. Canary C14 measured a first-run
  cold start refetching all 70 events with `nip77Behavior` reporting
  `behaviorally_proven` and `nip77Handoff` never leaving `none`;
  reconciliation engages on the NEXT request (a reconnect replay), where the
  same divergence costs 10 events instead of 70.
- **NIP-77 reconciliation is invisible per query (#1888).** Negentropy
  coverage is attributed through the same `attribute_eose` path as an ordinary
  EOSE, so `SourceEvidence.reconciledThrough` and `SourceStatus` are identical
  whether a result was reconciled or refetched. The only public distinguisher
  is the engine-global, per-relay
  `RelayDiagnostics.nip77Advertisement`/`nip77Behavior`/`nip77Handoff` triple,
  and `nip77Handoff` is a transient an app must accumulate from
  `observeDiagnostics()` to see at all.

- **Two of NIP-22's three root shapes cannot be read back through the
  capability's own demand (#1876).** `commentThreadDemand(root:)` binds the
  root identifier to the `#I` tag whatever the root is, but the composer writes
  `E` for an event root and `A` for an address root. So commenting on a note —
  the app-shaped case — composes and publishes correctly and can then never be
  observed through NIP-22's own read door; the app must hand-build
  `NMPFilter(kinds: [1111], tags: ["E": ...])`, i.e. own NIP-22's tag
  vocabulary itself. Measured end to end against a real strfry process by
  Canary C11, whose second test is red until this is fixed. Every existing test
  of the demand, at every layer, uses an external NIP-73 root, which is why the
  two shapes built ahead of the behaviour were never exercised.
- **A NIP-73 web root does not survive its own round trip (#1878).** Composed
  as `Nip73.url`, it decodes back as `Nip73.general(value:kind: "web")`. Both
  name one page and produce one demand, but they are different cases of a
  `Hashable` enum, so `decoded.root == theRootIComposed` is false and an app
  keying comments by their root splits one thread in two. No public
  `iValue`/`kValue` accessor or canonicalising constructor exists on any SDK
  surface, so the only way to ask "same thread" is to build the demand from
  each and compare. Recorded by Canary C11, which asserts the demand equality
  and prints both values rather than freezing either shape.

- **The FIRST `requestRows` on a window is dropped (#1886).** Canary C6
  measured a window opened at `initial: 10` staying at 10 rows for a bounded
  45s after `requestRows(atLeast: 20)`, with the relay up and holding 150
  matching events, and the advance delivering `WindowLoad.returned(added: 0)`.
  It never self-heals, and re-issuing the SAME target is a documented no-op,
  so an app has no way to ask again from where it is — in a real feed this is
  the first scroll-to-bottom doing nothing. Deterministic 5/5 across
  `(initial, firstTarget)` of (10,11), (10,20), (10,50), (1,2) and
  (10,10)→(10,11), with 0ms/1s/3s settle beats, so it is neither a race nor a
  function of step size. Every LATER advance reaches its target exactly. Root
  cause is in the issue: `stage_history_advance` attaches wire handles without
  arming admission, and the runtime drops the stage turn's effects on the
  success path, so the advance's REQ never reaches the wire; the second
  advance only works because its commit supersedes the first advance's handle
  and `withdraw_wire_demand` arms admission as a side effect. C6's first-advance
  phase is red until this is fixed. A second, related fact recorded there:
  `WindowLoad.returned(added:)` is not a usable progress signal — across runs
  the same advance reported `added: 20` and `added: 0`, with the rows arriving
  in a later `.idle` batch.

- **A derived binding is proven to GROW; nothing proves it retracts.** Canary
  C4 (#1871) drives `NMPBinding.derived` end to end against a real relay: a
  feed over "my kind:3 contact list projected through its `p` tags" starts
  delivering a newly-followed author's notes with no app action. What is
  untested and unclaimed is the other direction — whether an UNFOLLOW (a
  replacement kind:3 naming fewer authors) retracts the rows that author
  already contributed. `RowDelta.removed` makes retraction expressible, so
  both answers are plausible and neither is written down.
- **Suspend/resume transparency (#4): the on-device pass is pending.** Transport
  hardening (`SuspendGapDetector`/`apply_resume_gap`, wall-clock gap detection)
  and the clock audit of every suspension-spanning wait are done; what remains
  can only be closed by a human with a physical device — feed live, background
  10+ minutes (verified dead socket), foreground, confirm the feed catches up
  and diagnostics show re-established wire subs plus repaired coverage, with zero
  app code. Not reproducible in a simulator or headless test.
- **A relay's message on a SUCCESSFUL publish is discarded, and no app can
  reach it.** The frame's text survives as far as
  `handle_write_ack(event_id, status, message, ..)`
  (`crates/nmp-engine/src/core/write.rs:5455`, fed the whole `RelayMessage::Ok`
  at `crates/nmp-engine/src/core/auth_transport.rs:1866`), and is then thrown
  away by classification: `classify_relay_ack`
  (`crates/nmp-engine/src/core/mod.rs:435`) returns the UNIT variant
  `RelayAckClass::Acked` for every `ok=true`, and also for `ok=false` with a
  `duplicate:` prefix — whose explanation is lost the same way. The `Acked` arm
  (`write.rs:5507`) commits the unit `PublishQueueAttemptOutcome::Acked`
  (`crates/nmp-store/src/lib.rs:1347`) and emits the unit
  `RelayState::Published` (`crates/nmp-engine/src/publish_queue/mod.rs:194`),
  so the text is in no store row, no `WriteFact`, and nothing across the FFI.
  Every OTHER answer keeps the relay's words — `Rejected { reason }`,
  `AuthFailed { reason }`, `RelayWaiting::BackingOff { detail }` — so success
  is the one outcome an app cannot quote. Carrying it means a payload on all
  four of those types, in that order; anything less stops at the store
  boundary. Canary's `ComposeView` renders the absence in words rather than
  substituting an empty string for a message that was never kept.
- **`Receipt` does not carry the event id.** `publish` returns a `Receipt`
  whose only identifier is `id`, the store-issued RECEIPT id
  (`Packages/NMP/Sources/NMP/Receipt.swift:55`, from
  `crates/nmp-ffi/src/facade/receipt_stream.rs:208`), even though acceptance
  has already frozen the event and `PublishQueueEntry.eventID` calls that id
  "the write's identity from acceptance onward". An app that wants to show
  what it just published must instead wait for a fact that happens to quote
  the id (`WriteFact.relay`, or `SigningState.signed`), or re-find its own
  entry in a `publishQueue` page. Canary's `ComposeView` harvests it from the
  facts and shows "not reported yet" until one arrives.
- **Direct-Rust unwindowed observation evidence is built; windowed and native
  SDK parity remain open (#718).** `Frame.execution` carries
  resolver/reducer/runtime-owned observation-scoped facts, but windowed
  observations deliver no execution facts and UniFFI/Swift/Kotlin do not yet
  expose the vocabulary. #718 stays open until those projections and their
  cross-SDK falsifiers land.

## Store & persistence

- **Backend candidates are not semantically qualified (#698/#699).** The
  reference event/publishing trace checks redb against independent expected
  outcomes and attaches a stable recovery digest to every process-death
  failpoint. Fjall, LMDB, and SQLite have not passed that full path. Redb
  remains the production baseline.
- **Fjall is only partially qualified (#818, under #701).** A real
  `RLIMIT_FSIZE`/`SIGXFSZ` journal-write failure was proven to behave correctly
  on pinned Fjall 3.1.7/3.1.8 (3.1.6 silently lost the transaction); that
  harness has since been removed from the tree. It qualifies exactly one
  behaviour of one pinned build — it does not qualify Fjall's semantics,
  maintenance, compaction settlement, performance, or production readiness, and
  does not select a database; the adapter stays blocked and production
  constructors stay redb-only. A later Fjall release needs a fresh source and
  fault audit; it cannot inherit this by semver.
- **Native SDKs cannot branch on store-failure kind (#1762, #881).** The engine
  classifies `PersistenceFault` and computes `requires_reopen()`, but that
  classification does not cross `EngineError`/`FfiError`; native SDKs project
  `store_degraded` as an unstructured string, so a native host can display the
  degraded interval but cannot branch on the failure kind the way a Rust caller
  can.
- **Boot recovery still READS per intent (#889).** Reopening an engine
  rebuilds volatile write ownership before the first command, and two
  unnecessary durability barriers were removed (boot fell from 38.9s to 108ms),
  but recovery still visits every open intent, so its READ work and in-memory
  rebuild remain linear in durable-queue size. There is no incremental or
  interleaved recovery — a command arriving mid-rebuild would read a partial
  queue, so the rebuild stays one indivisible step. Acceptance-time
  retirement of superseded never-attempted obligations is what keeps that
  population small.
- **NIP-11 cache is process-local.** Acquisition, single-flight, validators,
  freshness, LRU at 256, and the copy/flight/waiter amplification bound (#467)
  are built, but the cache is deliberately in memory for this first contract; a
  cold process does not reuse the prior process's relay document. Persistence
  is a separate later decision.

## Protocol modules

- **NIP-65 bootstrap-publication helper is direct-Rust only (#764).**
  `nmp-nip65` ships engine-free plus an installable routing provider and native
  facade assembly, but the explicit kind:10002 bootstrap-publication helper has
  no native projection; native apps must not hand-roll that separate operation.
- **`nmp-blossom` covers the BUD verbs and their FFI/Swift/Kotlin projection,
  but not the composition layers (epic #216).** The `get`/`media` endpoints,
  NIP-68 `imeta` picture events, and the upload-then-publish composition seam
  are tracked follow-ups (#545/#551/#555).
- **`nmp-nip68` owns kind:20 build/decode with `imeta` provenance, but not the
  composition, FFI/Swift/Kotlin projection, or richer tag layers (#558, epic
  #216).**
- **`nmp-media` provides the standalone staged composition seam
  (prepare → upload → compose) with separated failure domains, but not the
  durable upload, the FFI projection, or BUD-03 server-list placement (#559,
  #562, epic #216).** The upload half is not crash-durable; the
  engine-integrated durable-upload obligation is the additive #562.
- **No general protocol-composer catalog; no kind:1-first core catalog.**
  Composition is selectively built: modules claim only exact NIP-defined
  schemas and typed contextual operations may add their own tags/route facts
  to foreign-schema builders. This is a scoping decision, not an oversight.

## Platform & runtime qualification

- **No iOS simulator/device runtime is claimed (#1401); no Android
  running-engine/lifecycle proof (#832–#833); no public package-repository
  release policy; no automatic installation of Xcode/Android SDKs.** Native
  product preparation (`.nmp.toml`, `nmp init/prepare/verify`, cached
  Apple/Kotlin/Android products) is first-class; runtime qualification remains
  platform work.
- **Broad multi-platform UI remains open (#75, #561).** Still unbuilt: broad
  Compose UI parity and a Compose Gallery, broader registry/template breadth,
  NIP-25 live reaction resources/write intents (#155), and broader
  product/photo/highlight/media component families.

## Reducer invariants without falsifiers

- **I3 and I8 have no reducer-level falsifier.** Both are fail-open guards on
  abnormal paths: I3 is the exact-generation session conjunction, I8 the
  per-turn `retry_scheduler_blocked` reset. Breaking either leaves the corpus
  green (a mutated reducer becomes strictly more permissive with no test
  noticing). Blocked on the hostile/degraded-input fixture tracked in #1736.
- **The `attach_wire_handle` ordering change is unobservable.** Indexing a
  handle before retaining its atoms vs reversed gives the entire workspace
  green either way — no reachable input distinguishes the orders, because the
  evidence refresh it protects only fires when a covered-atom reattach
  transfers request metadata, and nothing produces transferred claims today.
  Enforced only by a `debug_assert!` precondition, not a behavioural test.
- **`abandon_sub` call-site asymmetry — reframed, not audited.** The question
  "does every path that retires a subscription go through one door" stops being
  askable once the retiring field cluster (`active_request_evidence`,
  `active_request_revisions_by_sub`, `live_wire_requests`,
  `pending_request_evidence` — the field census's next owner candidates) has an
  owner and the door becomes the only way in. The fix is the extraction, not a
  call-site audit against line numbers that have already rotted. Re-open as a
  real finding only if the extraction surfaces a caller that cannot go through
  the owner's door.

## Process / tooling

- **Unexplained workspace-test abort (2026-08-16).** A `cargo test --workspace`
  aborted after 198 tests once; every subsequent full run has been clean
  (2031–2033 passed). A flaky assertion cannot produce it (libtest catches
  panics per-test and continues). Evidence points at resource exhaustion: a
  same-day 100GB `target/` with 1Gi free reproduced ENOSPC compiler deaths
  (phase-mismatched — those name compiler errors, not a mid-stream stop), and
  run-phase OOM `SIGKILL`/stack-overflow/double-panic is the closer
  mechanistic match for "the stream just stops" but has no direct evidence.
  Not reproduced. The settling experiment: a machine with real headroom and a
  fresh `target/`, pre-build every binary, drive free disk near-zero, run
  `cargo test --workspace --no-fail-fast`.
- **A panic in a runtime-owned background thread is silently swallowed.**
  `engine_thread.rs` and `pool/worker.rs` discard join results
  (`let _ = handle.join()`); with no `panic=abort`, a panicking thread neither
  kills the process nor surfaces anywhere — a test can pass while the runtime
  thread underneath it died. A masked-failure risk in exactly the fail-open
  class the reducer's own guards are audited for; unaudited.
- **One test asserts against the wall clock.**
  `crates/nmp-engine/tests/core_headless/live_queries.rs:185` asserts
  `start.elapsed() < 30s`; on a loaded machine that is a genuine flake. The
  standing rule is to control clocks rather than race them.
- **`process::exit` call sites are unaudited** for reachability from a test
  process. Present in `nmp-store/src/redb_store/{store,tests,postings_store,
  publish_queue_ops}.rs` and `nmp-cli/src/main.rs`. Flagged, not cleared.
- **Cross-SDK parity has no mechanical check (#1637).** The invariant — an app
  on one platform must not silently lose an operation the other two have — is
  real; the mechanism is not. The previous word-bag script was deleted
  (mutation testing found no falsifier; it passed a Swift SDK reduced to one
  comment-only file). The replacement — a checked-in manifest of exported
  UniFFI items generating a protocol each SDK must conform to, so a missing
  operation is a compile error — is separate, not-yet-started work under #1637.