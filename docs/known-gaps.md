# Known gaps & deferred follow-ups

Honest running list of open problems and deliberately-deferred work, so nothing
hides. Each entry is a current unresolved item with its consequence and owning
open issue. Fixed items are deleted (git/history remembers them), not narrated.

## Capability & signing

- **Three capability crates resolve the author themselves, which the owner
  ruled against on 2026-08-17.** `nmp-nip02`'s `set_following`,
  `nmp-bookmarks`' write doors and `nmp-nip29`'s group-list doors each carry
  their own copy of "read the session, take `current_pubkey`, refuse when
  signed out, stamp `Identity::Explicit`" — the universal resolution the write
  plane already owns, re-implemented per crate. `nmp-nip29`'s `Group` doors
  break the same rule from the other side by requiring `author: PublicKey` on
  every operation. The ruled shape is an optional account defaulted to the
  current one, resolved by the write plane; see
  `docs/internals/writes/identity.md` §7.

- **A NIP-42 challenge on a session bound to no identity is dropped, by
  design.** `authenticate_as: None` is a connection that declares no identity,
  so there is nothing a kind:22242 proof could be signed as and
  `on_auth_challenge` returns without consulting any policy. Routing it to the
  current account instead would be a different feature — an identity
  *discovered* rather than *declared* — and would credit an unbound session's
  coverage key to an identity it acquired mid-connection. An app that wants a
  read authenticated says so, with `authenticateAs`.
- **AUTH-policy callback inversion still open (#783).**
- **Session storage is app-owned: NMP ships no plaintext checkpoint and no
  automatic session store.** Transactional app-owned session storage tracked
  in #1398.
- **Permanent signer connection/correlation counters are not in engine
  diagnostics.**

## Routing & limits

- **There is no indexer write lane.**
  The 2026-08-17 routing ruling (`docs/internals/routing/outbox.md`) is that
  indexers always receive kind:0, kind:3 and kind:1xxxx events. Nothing
  publishes to an indexer today: the built-in `Auto` write resolver
  deliberately does not choose indexers, and indexers appear in the shipped
  lane vocabulary only as a read-side discovery input. The other two lanes in
  that ruling — the author's outbox and the operator's app relays — are built
  on both the read and write sides. This one is a ruling with no
  implementation, and no design for one.

- **Boundedness is only partial.** Indexed queries, router caps, and the
  expandable observation window are bounded, but
  graph, derived-set, wire, relay, ordinary-result, receipt, ingestion, and
  scheduler bounds do not share an explicit shortfall contract. Silent first-N
  behavior is forbidden (guarantee #17).
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
  Needed for reading NIP-17 DMs / private NIP-51 items (guarantee #12); #6 owns the
  async sign/encrypt/decrypt capability design that would supply both halves.

## Observation & delivery

- **Engine-lifetime memory grows linearly in DISTINCT filters observed, and
  closing the observation never releases it (#1846).** Repeated open/close
  cycles with a new filter each time grow memory with no plateau, confirmed at
  three cycle counts; the identical loop with the SAME filter is flat, so this
  is keyed on the filter being new, not on open/close overhead. Released in
  full by `shutdown()`. Whether it is an unbounded in-memory map or the
  store's page cache holding durable per-filter coverage rows cannot be
  determined through the public API — `DiagnosticsSnapshot` exposes no
  retained-bookkeeping or memory fact at all.
- **NMP does not reconcile at all; every read is a plain REQ.** NIP-77
  negentropy was deleted on the owner's instruction. It never engaged on a
  cold start (the behavioral probe resolved after the first REQ was already
  placed, so the first run always refetched in full), it had no app-facing
  door — three diagnostics strings and one terminal request variant, none of
  it app-constructible — and per query it was indistinguishable from a
  refetch, because its coverage was attributed through the same
  `attribute_eose` path. The prober, the reconciler, the four role
  subscriptions, the liveness sweep, the `nip77Advertisement`/`nip77Behavior`/
  `nip77Handoff` diagnostics triple and `RequestTerminal::Nip77` are all gone.
  Whatever replaces it, if anything does, starts from a clean surface.

- **Two of NIP-22's three root shapes cannot be read back through the
  capability's own demand (#1876).** `comment_thread_demand(root)` binds the
  root identifier to the `#I` tag whatever the root is, but the composer writes
  `E` for an event root and `A` for an address root. So commenting on a note —
  the app-shaped case — composes and publishes correctly and can then never be
  observed through NIP-22's own read door; the app must hand-build a `Filter`
  selecting kind 1111 with an explicit `E` tag binding, i.e. own NIP-22's tag
  vocabulary itself. Every existing test of the demand, at every layer, uses an
  external NIP-73 root, which is why the two shapes built ahead of the
  behaviour were never exercised.
- **A NIP-73 web root does not survive its own round trip (#1878).** Composed
  as `Nip73::Url(..)`, it decodes back as `Nip73::General { value, kind: "web" }`.
  Both name one page and produce one demand, but they are different variants of
  a `PartialEq`-derived enum, so `decoded == composed` is false and an app
  keying comments by their root splits one thread in two. No canonicalising
  constructor turns a decoded `General` id back into the `Url` variant, so the
  only way to ask "same thread" is to compare `i_value()`/`k_value()` directly
  rather than the enum value itself.

- **`WindowLoad::Returned { added }` is not a usable progress signal.** The
  SAME advance has been observed reporting `added: 20` on one run and
  `added: 0` on another, with the rows arriving in a later
  `WindowLoad::Idle` batch. `added` counts what one commit projected from the
  local store, which says nothing about what the advance will end up holding
  once the relay answers, so an app cannot use it to decide whether a scroll
  made progress. Wait on the delivered row count instead. (The FIRST-advance
  drop also recorded, #1886, is fixed: a staged advance now arms wire
  admission and the runtime dispatches the staged turn's effects on the
  success path.)

- **A derived binding is proven to GROW; nothing proves it retracts.** #1871
  drives `Binding::Derived` end to end against a real relay: a feed over "my
  kind:3 contact list projected through its `p` tags" starts delivering a
  newly-followed author's notes with no app action. What is untested and
  unclaimed is the other direction — whether an UNFOLLOW (a replacement kind:3
  naming fewer authors) retracts the rows that author already contributed.
  `RowDelta::Removed` makes retraction expressible, so both answers are
  plausible and neither is written down.
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
  so the text is in no store row and no `WriteFact`.
  Every OTHER answer keeps the relay's words — `Rejected { reason }`,
  `AuthFailed { reason }`, `RelayWaiting::BackingOff { detail }` — so success
  is the one outcome an app cannot quote. Carrying it means a payload on all
  four of those types, in that order; anything less stops at the store
  boundary.
- **`Receipt` does not carry the event id.** `publish` returns a `Receipt`
  whose only field is `id`, the store-issued RECEIPT id
  (`crates/nmp-engine/src/publish_queue/mod.rs:567`), even though acceptance
  has already frozen the event and `PublishQueueEntry::event_id`'s doc comment
  calls that id "the write's identity from acceptance onward". An app that
  wants to show what it just published must instead wait for a fact that
  happens to quote the id (`WriteFact::Relay`, or a signed `SigningState`),
  or re-find its own entry in a publish-queue page.
- **An observation reports no execution trace at all (#718).** The typed
  per-observation trace (`Frame.execution`, nine `ObservationFact` variants
  covering resolution, REQ placement, settlement, close, defer, withdrawal
  and mailbox overflow) is deleted. It was built for unwindowed observations
  only, no application ever read a variant of it, and the one thing anything
  consumed — a request settled on a relay — is now a direct
  reducer-to-`AuthorRouteProvider` effect that never reaches an app mailbox.
  An app that needs to know why a query asked the relays it asked has
  engine-global diagnostics and nothing query-scoped; #718 stays open on that
  question, not on the deleted shape.

## Store & persistence

- **The engine ships no retention policy (#1787, #1843).**
  The owner ruled on 2026-08-17 that it should have one: *"whatever, just ship a default policy;
  this is not a high priority."* Today canonical events are retained
  unconditionally — `RedbStore::gc` exists but no crate outside `nmp-store`
  ever calls it, and neither `GcRetentionSet` nor `GcReport` appears in `nmp`,
  so the documented remedy names a host that has no way to act.
  Terminal receipts meanwhile are evicted on a fixed, undocumented, unreachable
  24h/100k/256MiB policy from six call sites including the write path itself
  (#1843), on `SystemTime::now()`, so a device clock jump wipes terminal
  history. The store simultaneously refuses to evict what an app might want
  gone and silently evicts what an app might need kept. The ruling settles who
  owns the decision; the policy's shape, and the 24-hour wall clock that
  #1843 calls the part with no defence, are unbuilt. Tombstones are excluded
  by the 2026-07-11 permanence ruling. Disk size is explicitly not the reason
  to do this work — see `docs/builder/11-coverage.md`.

- **Backend candidates are not semantically qualified (#698/#699).** The
  reference event/publishing trace checks redb against independent expected
  outcomes and attaches a stable recovery digest to every process-death
  failpoint. Fjall, LMDB, and SQLite have not passed that full path. Redb
  remains the production baseline, and as of #1941 it is the only backend the
  tree contains: the Fjall and LMDB ceiling-ingest harnesses, and the
  `heed`/`fjall` dependencies that carried them, are deleted. Re-evaluating a
  candidate would start from `docs/design/storage-semantic-oracle.md`'s
  semantic trace and a fresh adapter, not from a harness that is still here.
- **Fjall is only partially qualified (#818, under #701).** A real
  `RLIMIT_FSIZE`/`SIGXFSZ` journal-write failure was proven to behave correctly
  on pinned Fjall 3.1.7/3.1.8 (3.1.6 silently lost the transaction); that
  harness has since been removed from the tree. It qualifies exactly one
  behaviour of one pinned build — it does not qualify Fjall's semantics,
  maintenance, compaction settlement, performance, or production readiness, and
  does not select a database; the adapter stays blocked and production
  constructors stay redb-only. A later Fjall release needs a fresh source and
  fault audit; it cannot inherit this by semver.
- **Boot recovery still READS per intent (#889).** Reopening an engine
  rebuilds volatile write ownership before the first command, and two
  unnecessary durability barriers were removed, but recovery still visits every
  open intent, so its READ work and in-memory
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
- **No blob-storage or media-composition capability.** Blossom blob upload
  (`nmp-blossom`), NIP-68 kind:20 picture events (`nmp-nip68`), the staged
  upload-then-publish seam (`nmp-media`) and the exact-byte asset identity
  they shared (`nmp-asset`) were all deleted on the owner's instruction: none
  had an application consumer. NMP has no media door on any surface, and epic
  #216 is not being pursued.
- **No general protocol-composer catalog; no kind:1-first core catalog.**
  Composition is selectively built: modules claim only exact NIP-defined
  schemas and typed contextual operations may add their own tags/route facts
  to foreign-schema builders. This is a scoping decision, not an oversight.

## Reducer invariants without falsifiers

- **I3 has no reducer-level falsifier.** It is a fail-open guard on an abnormal
  path — the exact-generation session conjunction. Breaking it leaves the corpus
  green (a mutated reducer becomes strictly more permissive with no test
  noticing). Blocked on the hostile/degraded-input fixture tracked in #1736. I8,
  the per-turn reset of the scheduler-suppression flag, was recorded here for the
  same reason and is retired rather than closed: the flag existed only to keep a
  latched store failure from becoming a busy-spin, and store failures are no
  longer latched.
- **The `attach_wire_handle` ordering change is unobservable.** Indexing a
  handle before retaining its atoms vs reversed gives the entire workspace
  green either way — no reachable input distinguishes the orders, because the
  evidence refresh it protects only fires when a covered-atom reattach
  transfers request metadata, and nothing produces transferred claims today.
  Enforced only by a `debug_assert!` precondition, not a behavioural test.
- **`CoordinateCoverage::Reconciling` no longer has a falsifier.** The
  `limit: 0` coordinate request that state names was only ever minted by the
  NIP-77 live-first barrier, and its falsifier
  (`a_live_first_barrier_never_publishes_over_an_unread_base`) drove that
  barrier through a real publish. NIP-77 is deleted, so the test could no
  longer reach the state and went with it. The classification itself is
  KEPT and is still correct: `coordinate_request_shape` reads the filter, not
  NIP-77 state, so an application live query carrying `limit: 0` over exactly
  one coordinate still lands on `LiveFirstBarrier`/`Reconciling` and still
  parks the semantic publish gate rather than publishing over an unread base.
  What is gone is the proof that it does. Re-establishing it needs a fixture
  that reaches the gate through an app-driven `limit: 0` query.
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
- **`process::exit` call sites are unaudited** for reachability from a test
  process. Present in `nmp-store/src/redb_store/{store,tests,postings_store,
  publish_queue_ops}.rs`. Flagged, not cleared.