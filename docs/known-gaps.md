# Known gaps & deferred follow-ups

Honest running list of things built-but-incomplete or deliberately deferred, so nothing hides. Each says who flagged it, why it's deferred, and when it must be closed. This is a truth-anchor companion to the bug-class ledger.

## Promoted v2 contract gaps - next work frame

The July 11 architecture promotion intentionally corrected several assumptions
after the original milestones. These are agreed target contracts, not claims
about current code:

- **Full Demand identity and scoped acquisition evidence are built
  end-to-end (#49/#714).** Rust, FFI, Swift, and Kotlin carry
  `selection + source authority + access context + cache + freshness`, including
  an independently declared complete demand at every `Derived.inner`.
  Context participates in graph/wire/coverage/evidence identity; native
  conversion has no filter-shaped fallback that can reapply defaults.
  Query snapshots carry compact per-current-plan source facts and explicit
  shortfalls, including populated `AwaitingAuth`, `AuthDenied`, and `Error`
  states. Diagnostics retains exact per-session/filter intervals as a distinct
  type; broader permanent-diagnostics expansion remains under #51.
- **Protected-session AUTH discovery is bounded, not proof that a relay will
  never challenge.** On each protected connection generation the transport
  observes until the socket first becomes readable or 250 ms elapse, whichever
  comes first. When readable, it drains that initial batch to a true
  `WouldBlock`; only an AUTH challenge in that drain is reduced before ordinary
  REQ/EVENT release. Public sessions skip this observation entirely. AUTH that
  arrives after the initial drain, including after a non-AUTH first frame,
  still parks protected work and supersedes the exact current AUTH epoch, but
  may follow an initial ordinary request or event. Asynchronous absence cannot
  be guaranteed without an explicit required-auth signal. This bounded
  liveness tradeoff is part of #8; security-policy enforcement remains outside
  that issue's scope.
- **Engine shutdown drains app-owned AUTH work, so a pending cancel hook that
  blocks forever blocks `EngineThread::join`.** The runtime's finite drain
  cancels every live policy/signer operation without polling and never blocks
  the engine thread on app code (cancellation is a nonblocking signal), but
  safe Rust cannot force-kill app code invoked synchronously by a pending
  operation's Drop cancellation hook while the runtime is being joined. A
  well-behaved `AuthPolicy`/signing capability — one whose pending cancel hook
  merely signals, or whose resolver eventually completes — drains cleanly; the
  symmetric property already holds for the sign-event completion path.
- **Whole-demand relay admission and fan-out limiting are built (#20).** One
  finite ceiling now covers the fully assembled read plan and the live
  transport worker set, including outbox, indexer, app, fallback, and explicit
  pinned lanes. Deterministic plan-time refusals are absent from executable
  wire work but remain visible as exact contextual `LocalLimit` query evidence
  and diagnostics; the transport boundary returns typed admission errors and
  preserves durable write lanes as explicit waiting work. Runtime
  reconciliation releases workers that have no current read, write, or
  ephemeral owner before dialing replacements. Access contexts remain
  physically isolated sockets, but a nonterminal write may time-share the
  same relay's Public slot under the finite ceiling (#598): reducer-issued
  read/write admission effects make the authority explicit, only write
  ownership may release Public, and terminal reconciliation restores any
  still-required read preamble. A protected read has no such authority, no
  different relay is evicted, and the configured physical-session envelope is
  never exceeded. Discovered local,
  private, link-local, and `.onion` targets remain rejected by default, while
  operator-configured and explicit contextual authority keep their separate
  trusted path. Other non-relay limit classes remain open under ledger #17.
- **Crash-safe acceptance, restart reattachment, bounded retry execution, and
  governed SDK observation are built.** This does not claim a finite retry
  count or retained attempt history; #46 owns that retention/GC gap. One transaction
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
- **The optional NIP-46 provider and governed sign-only paths are built; standard platform vault providers are not.**
  A current NIP-46 client now owns its independent signer-relay connection,
  NIP-42 AUTH, exact request correlation, `auth_url`, `switch_relays`, distinct
  communication/user keys, NIP-44 crypto, and frozen-event validation. Missing
  capabilities remain durable `AwaitingCapability`; a real redb close/reopen
  proof reconnects a bunker, promotes the exact frozen event, publishes, and
  receives a relay ACK. The protocol-neutral signer contract now lives in
  dependency-free `nmp-signer`; the explicit local-key implementation lives in
  `nmp-local-signer`, while the concrete remote protocol, relay/session, and
  checkpoint implementation lives in selectable `nmp-nip46`. Core FFI/Swift/
  Kotlin expose only one opaque signer mailbox. Separate NIP-46 FFI,
  `Packages/NMPNip46`, and Kotlin `:nip46` components project Primal discovery,
  one-click launch, package-filtered Android discovery, and an exact
  URI/package handoff contract. Deleting those provider packages leaves core
  and an unrelated external signer buildable. Supported native release builders
  fix the core-only or matched core/provider Cargo roots, use an isolated
  reusable target per package set, freeze the resolution after a locked fetch,
  and hold a per-build authorization only inside the managed Cargo subprocess.
  Before that authorization is revoked, the managed builder copies the exact
  outputs into a fresh artifact snapshot; Swift/Kotlin packaging reads only
  that snapshot, never the mutable Cargo target. Before exposing the snapshot,
  the builder extracts UniFFI's compiled metadata from the provider library
  and first requires positive metadata for both the core mailbox and provider
  proof. It then audits every callable namespace in that library, permits only
  the exact outward-only core source
  `nmp_ffi::NmpEngine::signer_mailbox`, and requires exactly one other entry
  carrying the core mailbox, with the compatibility proof in that
  constructor's inputs. No namespace is exempt: a linked crate that claims
  `nmp_ffi` is audited by callable shape like every other crate. The mailbox
  has private fields and its Rust constructor is crate-private, so even a
  linked namespace impostor cannot mint a mailbox for its allowlisted return;
  only the real core `NmpEngine` can vend one. UniFFI derives the audit label
  and no-mangle scaffolding symbol from the same module/type/function tuple,
  so a linked crate forging that exact return label collides with core at link
  time; the audit independently requires exactly one such source. This audits
  the same compiled authority bindgen consumes, independent of source
  location, claimed crate/module namespace, macro expansion, aliases, records,
  or other Rust spelling. It cannot prove the meaning of a raw integer that
  unsafe Rust later reinterprets as a mailbox pointer; such a `u64`/raw-handle
  escape remains outside the audit's guarantee and is not a supported entry
  shape.
  `build.rs` canonicalizes both
  the component root and its actual `OUT_DIR`, then requires the exact
  `<target>/release/build/nmp-ffi-*/out` layout, so nested targets, `..`
  escapes, and custom profile directories cannot inherit the authorization.
  Only within that fixed shape does the deterministic
  component identity self-derive and hash Cargo's exact resolved unit graph
  (package roots/edges, transitive features, profiles, and targets), every
  governed core/fixture input, and compiler/build inputs. An ad-hoc release
  invocation cannot accidentally write or replace a package input; concurrent
  managed builds of one package set refuse with a specific lock error rather
  than rotating each other's authorization. The kernel-held lock remains owned
  by any surviving Cargo/rustc descendants after a killed shell and releases
  automatically when the last holder exits; stale lock-file bytes alone never
  block a later build. Debug/IDE builds remain available
  but are not packaging inputs. External path patches
  that cannot be reproduced from the repository are refused. Swift and Kotlin
  compare the embedded identities before requesting the opaque mailbox, and a
  mismatch is a typed construction failure before any external Rust object
  crosses the component seam (#952). The build authorization is a working-
  discipline guard against accidental artifact substitution, not a secret
  against a caller deliberately forging the marker or reading/replaying the
  in-flight token; the native identity comparison is the runtime authority.
  The managed builder never returns or prints that token. Supported builders always use the exact release
  profile; custom/bench release-class builds are intentionally not a supported
  packaging path and fail without that builder authorization. The Android AAR
  work in #831 still owns
  publishing that same identity in provenance/Gradle metadata and
  emulator-qualifying a deliberately mismatched pair; the native check remains
  the final authority when packaging metadata is stale or tampered.
  Connections own scoped
  registrations, so a stale session cannot detach its replacement, and
  close/drop deterministically finishes only that session. An explicitly
  insecure SDK-owned plaintext file checkpoint now provides opt-in
  personal/development autologin (#197), while
  remaining distinct from the secure-provider contract. Still open under
  #47/#51: explicit per-write identity override, standard Keychain/Keystore
  providers and automatic secure-vault restore, NIP-55 execution/Android AAR
  integration, and permanent signer connection/correlation counters in engine
  diagnostics. The existing NIP-46 teardown, foreign-capability mailbox,
  pull-delivery, and process-global-runtime corrections remain separately
  tracked by #770/#783/#784/#871; this ownership split does not claim them.
  The governed component catalog and distinct snapshots cover both core
  `nmp-ffi` and the separately selectable `nmp-nip46-ffi` provider namespace.
  This is catalog/snapshot truth, not an independent-publication claim: #831
  still owns Android packaging and qualification.
  The sign-only operation now projects across Rust, FFI, Swift, and Kotlin:
  it binds an immutable request to the active registered signer, validates the
  exact returned event, remains bounded/cancellable, and creates no
  store/outbox/publication residue. NIP-07 origin prompts and browser
  networking remain host policy rather than engine behavior.
- **Protocol-module composition is built selectively, not universally.**
  `EventBuilder` is the grammar-level authorless value, and schema/context
  owners including NIP-22 and NIP-29 now return builders or closed
  `WriteIntent`s without acquiring an engine dependency. There is still no
  general protocol-composer catalog: modules claim only exact NIP-defined
  schemas, while typed contextual operations may add their own tags and route
  facts to foreign-schema builders. No kind:1-first core catalog exists.
- **NIP-29 Group publication is built for direct Rust; native publication
  remains absent ([#1015](https://github.com/pablof7z/nmp/issues/1015)).**
  `nmp_nip29::Group` owns `(host, group_id)`, mints host-pinned read demands,
  appends exactly one `h` to an app-selected `EventBuilder`, and returns an
  engine-routable explicit-host intent. `nmp::GroupOperations` sends that
  intent through the ordinary `Engine::publish` lifecycle and also exposes
  NIP-29-owned join/leave/moderation operations. FFI and Swift still expose
  only read-only `groupDiscoveryDemand`; they must not hand-roll `h`, routing,
  signing, or receipt behavior while [#1015](https://github.com/pablof7z/nmp/issues/1015)
  is open. No Kotlin or Android Group projection is claimed. The obsolete
  kind:9 composer and `[9,30315]` content catalog remain deleted:
  `nmp-nipc7` independently owns kind:9 and `q` replies, while
  mention/notification policy remains client-owned. `previous` remains omitted
  until a host-scoped, group-scoped, author-aware live-window capability can
  mint it without caller tuples, truncation, or transplantation.
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
- **Expandable observation windows are built; durable resume, global end, and
  product adoption are deliberately separate (#474, #485).** Windowing is a
  policy on the one read noun: `observe(query, window)` with
  `Window::Expandable { initial, max }` owns one canonical
  `created_at DESC, event_id ASC` active partition, exact exclusive tie-second
  paging, live rebalancing, scoped evidence, mechanical `WindowLoad` facts, and
  deterministic withdrawal across ordinary demand, the store, the runtime,
  UniFFI, Swift, and Kotlin. Growth is the declarative, monotonic, idempotent
  `request_rows(at_least:)`; delivery mode derives from boundedness (unbounded
  observations deliver exact rebased delta transitions — intermediate reducer
  emits may conflate, while full-set redelivery remains the known O(rows²)
  class — and windowed observations deliver conflated authoritative snapshots),
  and the host cannot construct a timestamp cursor, hold a
  continuation token, assemble independent pages, or retain an SDK backlog
  beyond the declared `max`. Store/read and transport failures remain typed and
  never become a false `Complete`/`End` or fabricated retraction; `AtBound` is
  a frame fact, never an error. Intentional v1 limits remain visible: a window
  target is not a durable restart token, no load fact claims a network-global
  end, and app presentation may reorder rows but cannot redefine cursor
  membership. 29er's scroll anchoring, display order, end-state policy, and
  on-device adoption remain in pablof7z/29er-next#77 rather than moving into
  NMP.
- **The optional parser/reference substrate and first SwiftUI component-owned
  acquisition slice are built; broad multi-platform UI remains open (#75,
  #561).** `nmp-content` is now a source-ranged plaintext/Markdown parser with
  no engine, query lifecycle, kind:0/NIP-23 codec, or hydration budget.
  Core NIP-19/NIP-21 decoding preserves the exact `npub`, `nprofile`, `note`,
  `nevent`, or `naddr` variant and its authored hints, but owns no kind:0
  mapping, source authority, relay admission, or demand planner. Direct
  Rust/FFI/Swift/Kotlin consume one shared locator corpus proving that exact
  parity plus malformed/secret refusal. Kotlin stops at the parser/locator
  boundary until a real Compose content surface exists; it has no replacement
  content session or acquisition helper.
  `NMPUI` walks immutable documents, offers literal components that open zero
  handles, and passes the exact locator to an app-supplied
  locator-to-`NMPDemand` resolver only after a selected profile mention/default
  event loader asks to observe. Each selected component owns one independently
  cancellable ordinary observation. `observeWhileVisible` releases only that
  component's handle while retaining the last snapshot;
  recursion uses immutable cycle/depth context with no active/resolved count
  coordinator. The outer event loader is replaceable independently of
  actual-row-kind/purpose dispatch, and unknown kinds retain a generic fallback.
  Exact profile and NIP-23 decoding remain gaps for their own protocol owners
  (#208 for kind:0); the raw-event fallback is honest in the meantime.
  Independently, #565/PR #577 has landed `Freshness::Live`/`MaxAge`/`CacheOnly`
  as a per-handle axis over coverage watermarks, enabling cache-only/consent and
  staleness-tolerant custom loaders without reintroducing a content coordinator.
  The SwiftUI family still includes Avatar/Name primitives, three mention
  treatments, generic event chrome, distinct portrait/Medium article cards,
  three user-card layouts, and three reaction families. NIP-02 remains the
  first component whose protocol resource/action also ships: `NMPFollowing`
  projects canonical kind:3 state, `NMPEngine.follow`/`unfollow` own
  source-evidenced tag-preserving guarded replacement, and `NMPFollowButton`
  only renders and forwards the tap. Pure documents plus literal or injected
  component factories make deterministic conformance states possible without
  a shared fake session. The native iOS Gallery consumes the exact components,
  explicitly owns its profile/event/address demand mapping, configures only two
  indexers for its live proof, and separately exercises
  literal/no-fetch policy, cycle/depth, unknown-kind, Dynamic Type, RTL,
  reduced motion, dark appearance, long Markdown, and 72-row rapid visibility
  churn while reporting engine wire-subscription evidence rather than a UI
  claim counter. A real loopback parity proof drives follow, duplicate
  no-op, unrelated-contact preservation, and unfollow through direct Rust and
  the iOS FFI surface. Controlled relay identity/list primitives now ship in
  SwiftUI and a narrow optional desktop-JVM Compose subproject (#198). Both
  render caller-supplied one-shot NIP-11 state and query-scoped `SourceStatus`;
  they own no engine, HTTP, polling, cache, timers, or image loading. The
  Compose proof is not broad content-component parity and does not qualify an
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
  router caps, and #474/#485's expandable observation window are bounded, but
  graph, derived-set, wire, relay, ordinary-result, receipt, ingestion, and
  scheduler bounds do not yet share an explicit shortfall contract. Silent
  first-N behavior is forbidden.
- **NIP-11 cache is process-local.** The engine now owns bounded one-shot
  acquisition, per-relay single-flight with shared async waiters, HTTP
  validators/freshness directives, typed advisory limitation claims, raw JSON,
  stale-on-error, explicit refresh, and least-recently-used retention at a
  strict 256-document bound. Refreshing last-good documents count toward that
  bound and remain available for stale-on-error; if all 256 are refreshing, a
  257th fresh result is delivered but not retained. Fetches are cancellable
  async tasks on the engine-owned shared runtime (#704) and use Hickory DNS plus
  HTTP under one three-second deadline; no caller-visible waiter or worker
  admission refusal exists, and engine shutdown closes every waiter and drains
  the bounded tasks even when app handles survive. Relay URL credentials are rejected before
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
  Swift wrapper tests run on the macOS host and exercise hostname NIP-11
  acquisition, async MainActor progress, and typed malformed-document refusal
  through the public API. Master packages the complete device + simulator +
  macOS XCFramework slice set, but no headless gate claims iOS runtime behavior;
  physical-device qualification remains separate. The Kotlin package remains a
  desktop-JVM projection; this work does not add or qualify an Android AAR.
  **The hidden cache/flight/waiter copy amplification is closed
  (#467).** One immutable payload owns the parsed document (including
  structured maps), exact raw JSON, and revision; cache entries, refreshing
  workers, 304/stale metadata versions, and all waiters share that payload.
  Runtime capability projection extracts only its compact evidence and retains
  no raw body. One engine admits at most 8 live distinct-relay HTTP/DNS/body
  flights, each capped at 256 KiB; excess callers wait cancellably in their own
  futures and same-relay callers subscribe to one shared completion instead of
  accumulating service-owned waiter records. The exact hidden raw-body envelope
  is therefore `(256 cached bodies + 8 live bodies) * 256 KiB` (66 MiB), not
  caller concurrency times 256 KiB. This is deliberately a raw-body bound, not
  a total-RSS claim: parsed values, relay URLs, HTTP metadata, maps,
  allocator/container overhead, caller-owned futures, and channel scaffolding
  are additional costs outside that number. The
  supported Rust facade still returns its existing ordinary owned value and
  UniFFI/native records remain owned; an application that concurrently
  materializes and retains 64 results owns those 64 copies by contract. A
  facade falsifier proves dropping them leaves exactly the one cached engine
  payload rather than a hidden waiter/result shadow cache.
- **~~Destructive trust-domain reset is missing as a defined contract~~ CLOSED
  (#232); ~~live-store deletion is only refused in-process~~ CLOSED (#489).**
  `Engine::reset_persistent_store`, the UniFFI operation, and the Swift/Kotlin
  `NMPEngine.resetPersistentStore` projections idempotently remove one unowned
  canonical store. `RedbStore::open` owns the guard at the lowest governed
  store layer: it acquires one nonblocking CROSS-PROCESS exclusive lock on a
  durable sidecar keyed by the resolved canonical target before the database is
  created, exposed, or mutated, and holds it by RAII until the database handle
  finishes dropping. There is no process-global path registry or refcount. A
  second owner -- same path, relative alias, existing symlink, dangling final
  symlink, this process or another -- is typed
  `RedbStoreOpenError::StoreAlreadyOpen` / `EngineError::StoreAlreadyOpen`,
  projected to Swift `storeAlreadyOpen` and Kotlin `StoreAlreadyOpen`. Path
  resolution is coherent when a missing final component appears
  (#1001): if `canonicalize` first observes `NotFound` but metadata then sees
  the ordinary file, the resolver restarts within its existing 40-step bound
  instead of returning stale `ENOENT`. It never retries the whole store open
  or accepts a third outcome; deterministic resolver and eight-opener
  falsifiers prove one owner and seven typed refusals.
  Production open acquires a second, required lock on the actual database
  inode; unlike redb's permissive default backend, an unsupported target lock
  fails before database initialization. Reset acquires that SAME pathname
  ownership, then joins NMP's target-inode lock and holds both through removal.
  A live hard-link alias therefore returns typed
  `StoreStillOpen { path }` without touching either name. A closed target with
  more than one hard link fails as `StoreResetFailed` before mutation because
  unlinking one name cannot prove the promised physical erasure. Existing and
  dangling final symlink paths resolve to the store target; reset never
  unlinks the alias inode. Reset clears cached
  events, pending writes, receipts, coverage/evidence, and related persisted
  state. Separately configured platform account checkpoints remain outside the
  store path and untouched. **Remaining boundary:** arbitrary external
  retargeting of the containing directory, or an uncooperative process creating
  another hard link after the locked final validation, is still a deployment
  concern callers must coordinate.
- **Public syntax remains provisional; its promotion protocol is now enforced.**
  Pinned snapshots cover the canonical `nmp` Rust facade and every active
  language-independent UniFFI proc-macro component in the closed-schema
  `docs/surface/components` catalog. CI requires exact
  regeneration and a schema-complete append to `docs/surface-change-log.md`
  whenever either baseline moves, and rejects historical log edits/deletions.
  Hand-written Swift/Kotlin public wrapper paths and their consumer-visible
  package/build/settings manifests are governed directly even when generated
  snapshots do not move. Per-component parity cannot be masked by vocabulary
  in another component. Declared Swift/Kotlin package roots and UniFFI
  namespaces are stable, while a valid active component can move Cargo package
  or build owner through the same governed regeneration. The catalog checks
  package/library names against the exact manifest; the extractor refuses
  undeclared or duplicate compiled namespaces. Catalog-order and reverse-order
  regeneration must produce byte-identical facade and component snapshots.
  The catalog and component extractor are separately locked, base-trusted tools
  outside the product workspace. The trusted
  checker/workflow is loaded from the PR base so a proposed head cannot replace
  its judge. Governance-program changes still require the explicit
  owner-controlled bootstrap procedure.
  The Rust baseline also resolves definitions of dependency-owned types
  explicitly re-exported by `nmp` (#89), recursively including dependency-owned
  types reachable through variants, fields, aliases, and signatures, so those
  shapes—and public inherent constructors/methods on the explicitly re-exported
  root definitions—cannot move behind an unchanged opaque `pub use`; unrelated
  mechanism APIs, nested helper impls, and trait/auto/blanket impls remain
  outside the supported snapshot.

## Load-bearing for M5 (the falsifier app) — must close before M5 claims pass

- **~~Protocol-coupled mutable relay directory~~ CLOSED-AS-DELETION (#870).** `RelayDirectory`, `LiveDirectory`, discovery-kind/indexer routing, protocol lanes, half-mutators, and core kind:10002 parsing are deleted. Generic router/core now read one neutral `Unknown | Present { outbound, inbound } | Absent` author fact keyed by decoded `PublicKey`; production mutation is one private, borrowed, non-cloneable atomic writer. Provider needs leave core generically, including settled zero-route contributors when an `Auto` has no destination. The engine-free `nmp-nip65` crate owns exact demand, canonical winner selection, marker parsing, admission, and all-source settlement; the non-default `nmp/nip65` feature privately assembles it through ordinary query/write values. Core-only dependency trees contain no NIP-65 crate. Native publication/packaging remains tracked by #952/#824.

- **~~Publish payload is unsigned-only across FFI (M4)~~ CLOSED (#32); verify placement updated (#52 Unit A0/Unit B).** `FfiWritePayload` now has `Unsigned`/`Signed` variants (mirroring `nmp::WritePayload`); a caller holding an already-signed event (external signer / NIP-46 bunker / verbatim republish) submits `.Signed` and the engine publishes it verbatim -- no re-sign, no tag mutation, no id recomputation. The verify moved OFF the FFI boundary (#52 Unit B): `convert::signed_event_from_ffi` only PARSES the reconstructed event's fields now (typed `FfiError::InvalidSignature` for a malformed sig hex, `InvalidEventId`/`InvalidPublicKey`/`InvalidTag` for other malformed fields) -- there is no `FfiError::InvalidSignedEvent` anymore. `nostr::Event::verify` instead runs at `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary (Unit A0/#56), so the guarantee holds for every entry point (this facade, direct-Rust, `from_parts`), not only FFI. A tampered `Signed` event parses fine at the FFI boundary and is rejected downstream, surfacing as `WriteStatus::Failed` -- the first and only status -- on the receipt stream, never a synchronous `FfiError`. Swift/Kotlin `WritePayload`/`WriteStatus` mirror both cases. Falsifier: `crates/nmp/src/engine.rs`'s `tampered_signed_publish_fails_closed_with_no_accepted` (direct-Rust), `crates/nmp-ffi/src/convert.rs`'s `ffi_publishes_presigned_event_verbatim`/`ffi_presigned_never_resigned`/`tampered_signed_event_still_parses_verify_moved_downstream`/`ffi_rejects_signed_event_with_unparseable_signature`, `crates/nmp-ffi/src/facade.rs`'s `ffi_tampered_signed_publish_fails_closed_on_receipt_stream`, plus `Packages/NMP/Tests/NMPTests/FilterBuilderTests.swift`'s `testSignedWriteIntentConversion`.

- **~~kind:10002 discovery over-fetch~~ SUPERSEDED (#870).** The old core-owned `sync_discovery` loop and its churn-specific tests were deleted with the mutable directory. The optional NIP-65 coordinator reroots one ordinary exact demand when the complete generic need set changes; zero needs unsubscribe, zero sources open nothing, and stale revisions cannot settle.

- **Unbounded historical replay can peg the main thread (M5 dogfooding finding), bound across two halves (#17).** `apps/Falsifier` (the M5 SwiftUI app) reproducibly saturates a simulator's main thread at ~97-98% CPU for 1-2 minutes, twice: (1) whenever a query without a `limit` (e.g. the app's `FeedFilters.followsRelayLists()`, `kinds:[10002]`) is freshly `observe`d, and (2) whenever `observeDiagnostics()` is first iterated. `sample` on the running process shows sustained top-of-stack time in `nmp_store::redb_store::RedbStore::EventStore::query` plus `serde_json`/schnorr-signature JSON parsing, not idle waiting -- real, repeated work, not a hang (it does eventually finish and CPU returns to 0%).
  - **Observation-delivery path CLOSED; broader limit work remains under #46.** `NMPQuery`/`NMPDiagnostics` used to re-deliver the full accumulated snapshot on every single delta (no batching/coalescing), so an ordinary app iterating `for await batch in query` with ordinary SwiftUI `@State` writes got many consecutive full re-renders, starving the run loop. The Rust ordinary-row edge also retained every reducer batch in an unbounded `mpsc` queue while a callback was slow. **Fix:** the producer now atomically composes skipped row deltas into one exact transition per changed event id in a single mailbox slot; applying the next delivered batch to the last delivered state yields the newest reducer state and latest evidence without full-set redelivery. Windowed rows and diagnostics already use one-slot latest snapshots. Swift now performs one native pull per app pull, owns no producer task or second queue, and cadence-limits snapshot returns to about one per 16 ms without prefetching; Kotlin likewise serializes native pulls and relies on the engine mailbox rather than `Flow.conflate()`. The backlog is bounded at every observation handoff, though an unwindowed query's semantic result cardinality and accumulated app state remain unbounded by design. Live-relay-verified (`Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`, real replay against `purplepag.es`/`relay.primal.net`) plus direct-pull/cadence Swift tests and the 10,000-skipped-update Rust proof establish final-state equality without a stale-frame queue. Ingestion pressure, graph/wire ceilings, and other broader #46 categories remain open.
  - **Rust query-cost half CLOSED (#38); per-event refresh cost now bounded — on-device re-verification pending.** `nmp-store`'s `RedbStore::query` used to decode every row's JSON with no index narrowing (the dominant `sample` cost). **Fix (#38):** two persistent redb secondary indexes (`BY_AUTHOR`/`BY_KIND`) maintained in lockstep through the one centralized `remove_row_in_txn`/insert path (so they cannot drift across supersession/kind:5/expiry/gc); `query` now does bounded index range-scans for id/author/kind/address filters and only JSON-decodes the narrowed candidate set (falsifier: an author-filtered query over 1 target + 200 noise rows decodes exactly 1). The other named cost — `crates/nmp/src/core/mod.rs` refreshing all handles after every ingested event — is unchanged, but each refresh is now a *cheap indexed* query rather than a full-table scan, so the O(events × handles) blow-up is bounded. **Honest status:** the root cause is fixed and the Swift-delivery half caps re-render frequency, but the ~97% CPU jank has NOT been re-measured on device with all three fixes (Swift coalescing + Rust index + churn) live — verify the running result on the Falsifier before declaring the M5 jank gone. Screenshots: `docs/screenshots/m5-06-diagnostics-loading-jank.jpg`, `m5-07-diagnostics-steady-state.jpg`.
  - **NIP-29 tag/limit amplification CLOSED at the store boundary (#142); device room-open verification pending.** `BY_AUTHOR`/`BY_KIND` still left `kind:9 & #h=<group> & limit:200` decoding every cached kind:9 event across every room, and the complete-set `EventStore::query` door cannot safely honor `limit` because reactive recompute and negentropy require its full answer (#124/#139). **Fix:** redb now maintains a generic NIP-01 single-letter tag index keyed by tag/value/`created_at`/event-id in the same transaction as every canonical mutation and rebuilds it crash-atomically on legacy reopen. A separate `query_newest` door reverse-scans one ordered tag bucket and stops after N accepted rows; handle projection uses that bounded door per root atom, then preserves the authoritative final merged global top-N. Real persisted corpus: 1,062 kind:9 rows, busiest `#h` room 557 rows, `limit:200`; 50-iteration release mean fell from 5.150 ms to 0.784 ms (6.57x), and full-event JSON/crypto reconstruction fell from 1,062 candidates to 200. This proves the store cost drop, not yet the end-to-end device UX; the remaining binary-record/planner/batch work is tracked under #148.
  - **Nested-JSON canonical event rows CLOSED (#150), then split immutable-note storage CLOSED (#162).** Canonical v3 rows are endian-defined binary values addressed by monotonic `u64` surrogate keys: immutable id/pubkey/signature/time/kind/tags/content bytes live in `EVENTS`, raw 32-byte ids resolve through `EVENT_IDS`, and relay/local provenance lives in a separate binary metadata sidecar. Every ordered/address/expiry index stores the surrogate key; canonical lowercase 64-hex tag values occupy 32 raw bytes in the tag index. Query predicates borrow fixed fields and tag/content slices from the redb value guard, so rejected candidates never construct `nostr::Event`, parse hex, or reconstruct secp types. An exact equal-or-earlier relay replay reads only the metadata sidecar and performs no write at all; signature adoption rewrites the immutable note only when the signed event actually changes. The v3 change is intentionally schema-breaking: opening a file containing a legacy event epoch now fails before any v3 table is created, so old outbox/coverage facts can never run beside an empty v3 event store. Differential matching tests pin equivalence with `nostr::Filter::match_event`, and a raw referential-integrity audit covers supersession, duplicate provenance, kind:5, NIP-40, GC, compensation, and every crash seam. On the 1,114-event real corpus (1,062 kind:9, busiest room 557), the bounded room query measured 0.260 ms versus the original 5.150 ms; a 1,114-event exact replay measured 6.102 ms versus 24.98 ms before the split, and 20 exact passes left the 4,214,784-byte redb file unchanged. The surrogate is a lookup/CPU win, not a claimed size win: v3 logical stored bytes were 1,486,162 versus v2's 1,474,770 (+11,392, 0.77%); its five query indexes were 475,137 versus 465,940 (+9,197, 1.97%) because exact tie ordering still retains the full id while each row gains an eight-byte value. The checked-in `storage_stats` example reproduces physical and per-table accounting across both schemas. Many *distinct* relays still grow and rewrite the variable-length sidecar: relay-url interning/fixed-width observations remain open under #148. These remain store microbenchmarks; end-to-end device room-open verification is still pending.
  - **Relay URL interning and fixed-width per-event observations CLOSED (#167).** This supersedes the final “remain open” sentence in the historical #162/v3 bullet above. Canonical v4 stores optional local intent state in a dedicated `NMPL` value and each relay observation as one fixed 12-byte `(event_key:u64, relay_key:u32)` key plus an eight-byte latest timestamp. Relay URLs are interned once behind bijective forward/reverse tables with exact refcounts; removing the last observation reclaims the URL, while monotonic relay keys are never reused. Exact/equal replay point-checks one observation and writes nothing; a later timestamp replaces one eight-byte value; a new relay adds one fixed row without rewriting event or local bytes. A transaction accumulates effective refcounts in memory and flushes the hot row once per distinct relay, including bulk insert, expiry, GC, supersession, and compensation. Query materialization joins observations only after borrowed event filtering and caches each parsed relay URL once per query. Every observation/event/relay/refcount relation is included in the raw exact-set integrity audit and a process-abort seam proves dictionary, observation, refcount, event, indexes, and outbox remain one atomic fact. The checked-in `ingest_bench` now reproduces a 1/20/100-relay matrix from a real current store, including busiest-room newest-200, complete and reopen-first queries, exact-replay growth, and logical/physical bytes. A three-run matrix on the 1,114-event corpus (1,062 kind:9; busiest room 557) measured 0.296/0.700/2.678 ms for room newest-200, 1.691/4.640/18.368 ms for complete queries, 4.943/5.969/5.998 ms for exact replay with zero file growth, and 1,437,260/1,862,424/3,652,664 logical bytes. At 100 relays the physical file was 16,809,984 bytes. For historical scale, the earlier v3 101-relay run measured 6.571 ms room, 36.008 ms complete, 30.523 ms per new-relay pass, 6,168,304 logical bytes, and 29,700,096 physical bytes. Public `Provenance` construction necessarily remains proportional to returned observations; the avoidable URL reparsing, variable-sidecar COW, and repeated hot-refcount writes are closed. Device room-open verification remains pending.
  - **Ordered one-best-index query planning CLOSED (#149); device verification pending.** The author and kind indexes are binary `(field, created_at, !event-id)` rows, joined by global-created-at and tag indexes with the same suffix. `query_newest` chooses one best index (author, the smallest tag value set, kind, then global time), reverse-scans newest-first, and applies every remaining filter to the borrowed binary event. #646 removed the redundant physical author+kind index; a combined filter chooses the smaller author or kind bucket and post-filters the other exact predicate. Single ranges stop directly at the requested visible limit; OR values are exact k-way merges with id deduplication and the canonical `created_at DESC, id ASC` tie-break. All index mutations remain inside the same crash-atomic transaction as events, coverage, and outbox state; the store defines one exact current schema epoch and refuses any other at open (#867), so there is no in-place index migration. Tests prove kind/global scans materialize exactly N rows, multi-tag OR order is exact, and rejected candidates stay borrowed. On the real 1,062-row corpus, 100-iteration release means were 0.373 ms for the busiest room, 0.299 ms for kind:9, and 0.317 ms for the global newest 200. The original room baseline was 5.150 ms; end-to-end device room-open remains to be re-measured.
  - **Cardinality-aware complete/bounded planning and streaming execution CLOSED (#169); device verification pending.** The shape-priority planner and complete-query candidate `HashSet` unions/intersections are gone. redb originally persisted exact live-row counts for global, author, kind, author+kind, and every single-letter tag/value prefix. One transaction-owned index bundle accumulates checked deltas in memory and flushes each touched prefix once in the same crash-atomic commit as canonical events and indexes; duplicate tags count one physical row, zero rows disappear, and an independently versioned sidecar rebuilds atomically by counting ordered index keys without dereferencing canonical event values before publishing its marker. Both complete and bounded reads generate every applicable bounded-fan-out plan, choose the smallest persisted physical bucket, retain one reverse redb iterator per OR prefix, heap-merge in canonical `created_at DESC, id ASC` order, and apply only unmatched predicates to the borrowed binary view. Multi-value overlap deduplicates the immediately repeated surrogate without an unbounded candidate set; bounded reads stop without advancing or dereferencing the next candidate after the visible limit. Exact instrumentation distinguishes index entries, borrowed event-value dereferences, and owned materializations. Tests include deterministic mixed filters differential against `MemoryStore`, empty-set/reversed-window semantics, selected-tag masking, overlapping tag OR, raw sidecar audit across every governed mutation/crash test, and missing-epoch rebuild. The checked `query_bench` measures complete/bounded global, kind, author, author+kind, tags, multi-tag selection, populated author unions, rejected-heavy search, and reopen-first reads. Follow-up #627 replaced the exact planner-only sidecar with a per-store-keyed uniform one-in-sixteen sample. The key prevents relay-controlled event-id grinding; all event/index semantics remain exact, zero/close estimates only affect which complete index is post-filtered, sampled deletion deltas stay atomic with index deletion, and a missing/old sidecar key triggers one atomic sampled rebuild before queries. On the representative 100,000-event corpus, fifteen production-path pairs measured sampled ingest 26.6% faster, while the query matrix's worst paired-median p95 change was +1.7%, under the 10% gate. #646 then removed the physical author+kind index and its sampled prefix row: full relay ingest improved another 13.3% with 25.3% fewer process writes; the worst paired-median query p95 change was +6.6%, still under the gate. End-to-end device room-open remains pending.
  - **Bounded interior `Derived` projections CLOSED (#187); device verification pending.** A `Derived` binding kept its inner NIP-01 `limit` in the descriptor and wire filter but used complete-set `EventStore::query` for local construction and recompute, silently turning “authors of the newest 200 matches” into a full-history materialization. The resolver now selects explicit limits through `query_newest` before applying its closed selector; unlimited derived nodes and negentropy retain the complete query door unchanged. A generic falsifier proves an older row outside top-N cannot affect the derived set, a newer row evicts exactly the old floor, and kind:5 retraction pulls the next-newest row back in. On the #186 million-row fixture, the real resolver subscription over a 59,915-row hot bucket fell from 3,786.191 ms p50 to 0.730 ms p50 (1.406 ms p95) while producing the same 33 demand atoms. This is generic resolver semantics, not NIP-29-specific storage logic; #176 still owns physical-device closure.
  - **Portable packed tag/string arenas CLOSED (#170); device verification pending.** Immutable event codec v4 keeps the 158-byte fixed header, then stores cumulative tag ends, one four-byte atom descriptor per element, a dense arena, and directly addressable content. Descriptors inline zero-to-three-byte UTF-8, point to shortest-form LEB-length UTF-8 cells, or point to raw 32-byte canonical lowercase-hex identities; borrowed tag iteration returns text/raw views, and each query prepares raw wanted values once for binary search, so rejected candidates neither allocate nor hex-encode. Full validation rejects overflow, gaps, overlap, unused arena tails, non-zero reserved/padding bits, overlong LEB, invalid UTF-8, empty tags, representation aliases, truncation, and trailing bytes. The encoder makes two classification passes but allocates only the final value; materialization alone recreates exact lowercase hex strings for returned rows. Unchanged local/provenance sidecars retain codec v3, composite displaced rows move to v4, and the whole crash-atomic store bundle moves to rejecting epoch v5: any v4 event/displaced table aborts open before one v5 table is created, with no compatibility path. On the preserved 1,114-event corpus (2,543 tags, 5,085 atoms; 2,535 inline and 1,348 raw32), immutable values are 881,779→837,122 bytes (-5.064%) and the events table is 890,691→846,034 stored bytes (-5.014%). Five identical paired event-only redb builds measured 2,670,592→2,584,576 compacted bytes (-3.221%); full-store compacted file size is deliberately not claimed because redb 4.1 compaction is bimodal under layout entropy. A tag-heavy NIP-29 falsifier is 1,487→959 bytes (-35.51%). Alternating same-session real imports measured median 34.97 ms for v3 versus 34.01 ms for v4, while the codec itself encoded all 1,114 events in 0.187 ms; paired room/member/global queries remained within run noise and exact results were unchanged. End-to-end device room-open remains pending.
  - **Parallel verification + single-writer batch ingest CLOSED (#151); one table bundle per governed batch CLOSED (#164); device verification pending.** Transport workers still feed one pool-global verified-id/signature cache, but the translator now drains bursts of up to 128 frames and runs first-seen schnorr verification concurrently on native targets (the same code has a sequential wasm fallback); known ids remain cheap signature comparisons. The runtime preserves frame order while coalescing queued frames into one resolver call. `EventStore::insert_batch` runs the exact governed insert state machine in input order inside one redb write transaction and commits once, including event rows, every ordered/tag/expiry/address index, kind:5 effects, provenance adoption, and outbox satisfaction; any persistence error aborts the whole batch. The v3 writer opens that transaction's canonical/index/outbox tables once and reuses the bundle for every event rather than reopening every table per row. The resolver reacts once to the combined insert/remove set and the engine recompiles/refreshes once per burst. Both backends share contract tests for input-order supersession/provenance equivalence. On the same 1,114-event corpus, one current-schema release import measured 22.575 ms versus the prior 29.8 ms all-event batch; batch size 128 measured 76.3 ms versus 103.2 ms. The checked-in `ingest_bench` also measures exact duplicate replay and physical file growth. This isolated the store transaction cost; the persistent-worker and end-to-end measurements it originally left open are superseded by #168 below.
- **Parse-once typed relay ingest and persistent bounded verification CLOSED (#168); device verification pending.** The websocket boundary now parses each text frame exactly once into a typed `RelayMessage`; EVENT payloads move immediately into `Arc<Event>`, and verifier workers plus the engine share that allocation until the engine unwraps it for binary persistence, so the old `Value -> event JSON -> Event -> original frame parse` chain, production first-seen deep clone, and transport's direct `serde_json` dependency are gone. Native verification uses a fixed persistent worker set with one reusable secp context and one bounded queue per worker; wasm keeps the same ordered API with deterministic sequential verification. Crypto runs outside `PoolInner`; every payload recomputes its event id exactly once before identical unknown `(id, signature)` pairs may share signature work, preventing same-batch or cached-id admission of mutated content/tags/time/kind. Generation planning applies same-batch reconnect transitions in FIFO order and the real state is rechecked after verification, so close/reopen cannot admit stale work; cache capacity is explicit. A failed verifier lane rejects its affected task, surfaces engine diagnostics without falsely incrementing relay-misbehavior, and is replaced before future batches. Worker-to-translator and pool-to-engine queues are bounded; engine transactions are independently capped at 128 frames; an applied acknowledgement prevents another relay batch from entering the engine until resolver/store effects finish. Shutdown disconnects an event-driven cancellation channel, releasing a bridge waiting for ack and any blocked bounded producer without polling; immediate durable-send failures resolve locally rather than re-entering the engine's own queue. Tests pin mixed-frame order including EVENT-before-EOSE, same-batch reconnect, stale generations, mutated/invalid/mismatched signatures, cache eviction, worker replacement, transaction caps, backpressure cancellation, shutdown behavior, and real relay reconnects. A checked release rerun over the preserved 1,114-event corpus measured 2.307 ms for single relay-message parse plus shared allocation, 6.752 ms for the honest full first-seen path (event-id recomputation plus persistent-worker signature verification), and 3.272 ms for known-redelivery event-id recomputation plus signature checks. The typed resolver-to-governed-redb harness measured 40.373 ms on the hardened tree versus 46.140 ms on the prior PR head in the same session; an earlier lower-I/O run measured 18.712 ms, exposing filesystem variance but no regression from this hardening. The direct wasm compile remains blocked before NMP code by the workspace's existing `getrandom`, `ring`, and `secp256k1-sys` target configuration; the source keeps a thread/channel-free wasm branch. End-to-end device room-open verification remains pending.
- **Transport/verifier OS-thread ownership CLOSED (#442, #446); native observation and internal adapter admission REPLACED by pull-based handles and async tasks (#680, #704).** Every engine owns exactly two persistent native verifier workers (one on wasm's sequential path), one transport translator, one relay-retirement reaper, and two shared async-runtime workers; there is no blocking-adapter pool or pool reaper. `max_relays` bounds demanded live relay workers plus an equal charged retirement allowance. **#680 removed the one-OS-thread-per-observation bridge and the app-visible native-task ceiling entirely:** row, window, diagnostics, follow, receipt, and follow-action streams are waker-driven async pull handles (`ObservationHandle::next()`) over engine-owned bounded mailboxes, so NMP OS-thread count is independent of live-observation count. `max_native_tasks`/`maxNativeTasks`/`ExecutorSaturated`/the native-task census/idle-barrier are gone from Rust, FFI, Swift, and Kotlin. Receipt/follow-action live facts use a fixed 32-item FIFO: overflow retains the prefix, prunes the stalled sink, and reports typed lag; receipt reattachment traverses deterministic durable pages of at most 32 facts using an identity-stable continuation bounded by relay fan-out, then atomically joins live work after a caught-up check. The cursor records consumed per-lane fact identities rather than a numeric offset into mutable reconstructed history, so a durable fact added between pages is delivered exactly once even when it sorts before already-consumed facts from another relay. Every live receipt delivery also has a private registration identity tied to the consumer FIFO's close/drop hook, so cancellation removes the exact sink without waiting for another status from a potentially permanently parked receipt. This bounds live delivery, **not** total persisted attempt history: durable receipt/attempt retention and GC remain open under #46, and replay currently reconstructs that retained canonical history before selecting each delivery page. **#704 removes the remaining internal admission concept:** NIP-11, signer, AUTH, follow-action, and NIP-46 logical work runs as async tasks; signer/AUTH completion doors are waker-and-condvar primitives whose enum lifecycles make cancellation, resolution, and receiver ownership mutually explicit; no logical wait holds a scheduler permit or worker thread, and no operation exposes `ThreadUnavailable`/`WaiterSaturated` because an internal scheduler is occupied. An admitted NIP-11 acquisition does hold one of 8 private physical network/body permits until completion; excess callers await that bound cancellably in their own futures. NIP-46 sessions share async execution while retaining only bounded per-session transport workers required to preserve distinct NIP-42 transport identity. Foreign completions whose contract permits blocking run on fresh per-operation threads rather than an admitted internal pool. Falsifiers cover a 1,000-handle thread-scaling proof (0 thread growth); a 64+-observation dense-composition proof; cancellation/shutdown wake-to-`None`; normal Swift loop-exit teardown; concurrent-`next()` misuse; fixed-size receipt lag followed by finite durable replay; mutation-between-pages exactly-once delivery; 128 close/drop reattachments on a permanently parked receipt with zero retained delivery registrations; dense mixed observe/NIP-11/sign/follow load without capacity refusal; 1/10/50/100 NIP-46 sessions with zero executor-thread growth; typed one-shot completion ownership; and public-surface absence of the deleted capacity vocabulary. `docs/design/async-observation-handles.md` and `docs/design/internal-executor-elimination.md` record the replacement architecture; `native-task-executor.md` is retained as the superseded record.
  - **Residual (documented):** under Kotlin *per-call* cancellation (`withTimeout { handle.next() }`) a delta-row frame whose `poll` returned `Ready` but was dropped before UniFFI's `complete` retrieved it can be lost, diverging that one unbounded row stream. It cannot affect self-contained snapshot streams (window/diagnostics/follow) and does not arise under the compliant idiom (cancel the collection → `cancel()` the handle). A peek/commit hardening is deferred.
- **Suspend/resume transparency (#4): transport-internal hardening + clock audit done; physical-device evidence pending.** iOS kills sockets when the app backgrounds; the requirement is that the engine make resume fully transparent (reconnect, replay, repair coverage) with zero app code, per the M4 kill condition against scene-phase/app-lifecycle machinery. `crates/nmp-transport/src/keepalive.rs`'s `SuspendGapDetector` (paired with `apply_resume_gap`, threaded through `pool/worker.rs`'s connected loop) detects a large gap between consecutive worker-loop iterations using a wall-clock (`SystemTime`) reading rather than `Instant` — Apple's `Instant` is `CLOCK_UPTIME_RAW` and does not advance across device sleep, so it cannot observe the gap at all, let alone measure it. On detection, an otherwise-`Idle` keepalive verdict is upgraded to an immediate ping (never double-pinging a ping already awaiting its pong), cutting the worst-case dead-socket detection window from the ~60s idle+pong keepalive cycle alone. A clock audit of every other suspension-spanning wait in transport/engine (reconnect backoff, the keepalive FSM's own idle/pong `Instant` math, the 250ms NIP-11 capability-decision grace, the NIP-11 acquisition path's `SystemTime`-based freshness/deadline math, and the engine's `next_deadline()`/`duration_until`) found no additional concrete bug: engine-level deadlines are already wall-clock (`nostr::Timestamp`) and already floor a past-due post-suspend deadline to an immediate tick, and every other `Instant`-based wait is a short, self-consistent relative timer inside a thread that is itself frozen for the same suspended interval, so it simply resumes correctly rather than drifting. **What remains open, and can only be closed by a human with a physical device:** the on-device pass itself — feed live, background 10+ minutes (verified dead socket), foreground, confirm the feed catches up and `DiagnosticsView` shows re-established wire subs plus repaired coverage, with zero app code. Runbook: `docs/plans/M5-ios-falsifier-plan.md` §6.1. Negentropy-under-long-suspension is verified observationally in that same pass (the reconnect-repair mechanism itself is already covered by #563); this is deliberately not a separate Rust falsifier, since the suspension-specific factors (stale TLS sessions, a changed network path, actual OS backgrounding kill semantics) are not reproducible in a simulator or a headless test.

## Real but non-blocking for the falsifier (feeds, not DMs)

- **~~DM inbox routing incorrect (M3-D)~~ CLOSED (#19), then removed (#839/#870).** The unsafe generic `WriteRouting::ToInboxes` remains deleted. Neutral `AuthorRoutes.inbound` is available to the built-in p-tag fan-out, but no protocol-specific inbox accessor or route vocabulary exists in the router.

- **Decrypt-result feedback path missing (M3-C, plan §8 item 2).** `Effect::RequestDecrypt` is an explicit no-op; there is no `EngineMsg` to feed a decrypt result back into ingest. Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12 encrypted-content path). Deferred with E/negentropy still open; not on the falsifier's feed path.

- **~~Reconnect replayed NIP-77 demand as plain REQ~~ CLOSED (#563).** Every new Public connection generation now clears the stale preamble and repeats the same gap-free sequence as an ordinary demand change: distinct live `REQ {limit:0}` → exact EOSE → concurrent Negentropy while live delivery remains open. Timeout/error paths retain live overlap and use role-separated full-backlog REQs.

- **~~No time driver for liveness/timeout sweeps (M3-E)~~ CLOSED (#39 via PR #42).** The engine loop's `cmd_rx.recv()` is now `recv_timeout(next_deadline − now)`, armed from `EngineCore::next_deadline()` (min over the store expiration index + neg-session liveness deadlines): zero new threads, wakes exactly at the next real deadline, blocks forever on `recv()` when none exist, re-arms every iteration (an ingest introducing an earlier deadline re-arms naturally — no interrupt machinery). NIP-40 expiry fires event-driven through the same driver. Review caught + fixed a ~1s 100% CPU busy-spin (the neg-liveness sweep threshold `> N` was misaligned with the armed deadline `started_at + N`; now `now >= started_at + N`, so the tick that fires the deadline also clears it); regression test (`neg_liveness_deadline_does_not_busy_spin`) hand-verified failing pre-fix at ~986ms CPU, passing post-fix. D8-clean, no polling.

## Design-level (validated from external feedback — see docs/reviews/2026-07-11-external-feedback-triage.md)

- **~~Supersession retraction blindness~~ LANDED (#195 via PR #227; #228 via PR #230).** The symmetric negative-delta lane described in `docs/design/retraction-and-negative-deltas.md` now carries exact inserted, removed, and provenance-growth facts from relay ingest, durable local acceptance, pre-signature compensation, and NIP-40 expiry through the resolver/engine boundary. Stable complete simple handles apply those committed facts without reopening their full result set; bounded handles retain exact top-N backfill, while derived, multi-root, Strict-cache, incomplete, demand-changing, evidence-changing, or otherwise unproven shapes conservatively keep the full-refresh oracle. Differential and counting-store falsifiers cover replaceable supersession, provisional kind:5 suppression/reveal, compensation restoration, expiry, duplicate/stale/refused no-ops, and mixed mutation sequences. The design document's optimistic-write details remain superseded: durable accepted rows use typed `Pending(intentId) | Signed(signature)` state, and only cancellation or terminal **pre-signature** failure retracts and compensates a displaced replaceable; relay rejection after signing changes receipt evidence only. `docs/design/durable-write-signing-and-retry.md` owns that correction. Permanent kind:5 tombstone retention is built under the owner decision recorded in #23; #176 still owns end-to-end physical-device room-open verification.

- **~~Four bounded correctness fixes from the external-feedback triage~~ LANDED (merge `9220f65`).** (1) Signature-verification gate at the network layer (`nmp-transport` frame seam) — kind-independent, verify-once per event id (redelivery string-compares the cached sig, no re-schnorr), invalid sig → drop + `RelayHealth::invalid_signature_count`; cache reads never re-verified. Makes ledger #5 honest. (2) FFI no longer panics on malformed `Literal` hex (typed error) and no longer silently drops malformed tags (`tags_from_ffi` returns `Result` — NMP can't sign a different event than the app composed). (3) `DescriptorHash`/`CoverageKey` widened FNV-64 → BLAKE3 256-bit (a network-controlled, durable-and-refcount key must be collision-resistant; a forged collision there would attach a watermark fact to the wrong filter). (4) `coalesce` never merges limited filters (relay-side truncation under-fetch), and a known-zero-write-relay author stops perpetual discovery.

## Persistence robustness

- **Backend candidates are not semantically qualified (#698/#699).** The
  backend-independent oracle now compares MemoryStore and Redb after every
  operation in the reference event/publishing trace, closes and reopens Redb
  at every checkpoint, and attaches a stable semantic recovery digest to every
  existing process-death failpoint. Fjall has not yet passed that full path or
  completed compaction settlement accounting; LMDB has not been tested with a
  packed backend-native Nostr layout; SQLite and browser persistence remain a
  separate portability track. Redb remains the production baseline.

- **Fjall journal write-error recovery is proven; everything else about Fjall
  is not (#818, under #701).** `tools/fjall-journal-fault/` runs a real
  one-shot `RLIMIT_FSIZE`/`SIGXFSZ` journal write failure against pinned Fjall
  3.1.6, 3.1.7, and 3.1.8 builds and records what a caller observes: 3.1.6
  returns `Ok` from `commit` and makes every batch key live in-process while
  the journal holds only a truncated batch, so reopen silently returns the
  pre-transaction state; 3.1.7 and 3.1.8 return the propagated journal error
  (`Error::Io(EFBIG)`) with no partial state in-process and reopen byte-identical
  to the pre-state twice over. The harness is a workspace member so
  `cargo test --workspace` runs it, while the three pinned release probes stay
  detached workspaces built as child processes.
  This qualifies exactly one behaviour of the pinned 3.1.8 candidate — an
  acknowledged transaction is not silently unrecoverable when the journal write
  fails. It does **not** qualify Fjall's semantics, maintenance, compaction
  settlement, performance, or production readiness, and it does not select a
  database; the adapter stays blocked and production constructors stay
  Redb-only. Linux is the only platform where the fault shape is claimed; other
  kernels are typed unsupported rather than skipped. A later Fjall release
  requires a fresh source and fault audit — it cannot inherit this result by
  semver. Separately, that work measured a Fjall transaction rejected with
  `Error::Poisoned` becoming durable after reopen when the underlying write
  fault clears before shutdown (with the fault sustained, it reopens to the exact
  pre-state); #821 owns that error contract and its oracle consequence.

- **Fallible ingest/read store doors landed; two read peeks and FFI plumbing deferred (#122).** The six ingest/read `EventStore` doors (`insert`/`query`/`remove`/`expire_due`/`record_coverage`/`gc`) now return `Result<_, PersistenceError>` — `RedbStore` propagates real redb I/O errors instead of `.expect()`-panicking on every EVENT frame, and the engine degrades the local cache to read-only (a `DiagnosticsSnapshot.store_degraded` signal) instead of crashing the host app. Deliberately NOT done in that change, flagged here so nothing hides: (1) `EventStore::next_expiration` and `get_coverage` (small index/coverage read peeks) are still `.expect()`-on-I/O — a disk error there still panics; widening them ripples into the engine's deadline-arming hot path and was scoped out. (2) `store_degraded` is surfaced only on the Rust `DiagnosticsSnapshot`; it is NOT plumbed through `FfiDiagnosticsSnapshot`/Swift/Kotlin, so a native host cannot yet observe the read-only degrade. (3) The degrade policy is intentionally minimal (latch first error, skip the reactive step, emit a diagnostic, keep running) — there is no recovery/reopen path, no per-door policy, and no bounded-retry framework.

- **Request-scoped facts-before-claims durability is built (#816).** The acquisition core retains one exhaustive coverage-authority state on each exact send snapshot. A failed EVENT transaction poisons the then-current possible owners of that exact `(RelaySessionKey, wire subscription id)` FIFO; a missing-id backfill targets its retained original NEG send; a later revision, different filter FIFO, relay session, or access context stays eligible. Post-commit projection/read failures and unrelated store doors still update the observational `store_degraded` diagnostic but never become coverage policy. Ordinary EOSE and NIP-77 completion both consume an owned completion through one door that resolves every retained shape before one request-level `EventStore::record_coverage` batch. MemoryStore mutates the full batch together; Redb stages and commits every row in one transaction. Real event/coverage before+after-commit process-death seams reopen only as no fact/no claim, fact/no claim, or fact/all claims, with a stable second-reopen digest. Poison is deliberately volatile with its request/session owner; there is no poison table or schema migration. Still open and unchanged: the #122/#895 recovery policy is only a first-error diagnostic latch, `get_coverage`/`next_expiration` remain infallible peeks, and native SDKs still do not project `store_degraded`.

- **Store failures are classified; in-place Redb reopen is NOT built (#895).** `PersistenceError` now carries a typed `PersistenceFault` and a `DurabilityOutcome` derived from redb's own error at the single `persist_err` funnel, so an embedder can tell a latched handle (`PreviousIo`/`DatabaseClosed` — raised before the backend op, therefore determinate-absent) from the originating `Io` failure (durability genuinely unknown: it may be pre-flush and absent, mid-flush and undecidable, or post-flush on an already-durable transaction) without matching on a message. The classification also renders into `Display`, which is what an embedder logs. What is deliberately NOT here: `RedbStore` still owns `db: Database` bare, with no close/reopen/reconcile door — recovering a latched handle still requires dropping the store and opening a new one, which for a daemon means a process restart. That step was ordered AFTER #790 (now landed — see the next entry), because a reopen-and-recover path walks straight through the persisted-row `.expect()` sites and the infallible `recover_outbox` that #790 removed; shipping it first would have produced a recovery path that panics the host it exists to save. Also not here: nothing propagates the classification into `EngineError`/`FfiError` (that boundary is #881), and the `nmp` acquisition core's `degrade_store` still latches only the rendered string.

- **Persisted-row decoding is fallible; two peek doors remain (#790).** Every production decoder of a store-owned Redb row now reports a malformed, truncated, or schema-incompatible value as `PersistenceError` through its owning `EventStore` door instead of `.expect()`-panicking the embedding host: coverage rows in `record_coverage`/`gc`, canonical event views and local provenance on the query/GC/rebuild paths, address tombstones, suppression claimant sets, outbox intents and frozen events, kind:5 claim records, displaced canonical snapshots, boot reconciliation, and the packed-postings dictionaries/segments/run catalog. `EventStore::recover_outbox` is now `Result<Vec<RecoveredIntent>, PersistenceError>`, and `EngineCore::recover_on_boot` degrades once through the existing #122 path rather than fabricating a receipt, lane, route, or wire effect from a journal it could not read. A decode failure is classified `PersistenceFault::Invariant`, not `Corrupted`: the backend is healthy and every fallible decode and validation precedes the commit, so `DurabilityOutcome::Absent` is provable rather than merely convenient. Deliberately NOT done, flagged so nothing hides: (1) `get_coverage` and `next_expiration` are still infallible at the trait, so `get_coverage` keeps exactly one `.expect()` on a coverage-row decode — widening those two peeks is #763's unit, which owns the caller changes. (2) The query path runs `DictionaryView::validate_order` (strict key ordering, constant memory) rather than the full `validate`; the id-uniqueness half needs a set sized to the run's dictionary, which would violate the same issue's bounded-query-memory constraint, so it runs on the compaction path every run passes through instead. (3) `allocate_run_id` proves the allocator against the run catalog and its `POSTINGS_RUN_BY_MIN` bijection and against one dictionary per live run, but does not sweep `POSTINGS_SEGMENTS`/`POSTINGS_DEAD_KEYS` for orphan rows on every publish — that is an O(families x shards x runs) scan on the ingest hot path, and an orphan segment is unreachable from a scan that only reads segments for live run ids.

## Observation execution evidence

- **Direct-Rust unwindowed observation evidence is built; windowed and native
  SDK parity remain open (#718, PR #721).** `Frame.execution` now carries
  resolver/reducer/runtime-owned facts for stable structural descriptor paths,
  exact resolved values and canonical NIP-01 filters, transport-accepted REQ
  and replay generations, attributed EOSE, relay close/refusal, withdrawal,
  and explicit bounded-mailbox overflow. This is real observation-scoped
  causality, not a projection of engine-global diagnostics. The current
  projection deliberately stops at unwindowed direct Rust: windowed
  observations currently deliver no execution facts, and UniFFI, Swift, and
  Kotlin do not yet expose this vocabulary. Issue #718 remains open until those
  projections and their cross-SDK falsifiers land.

## Security hardening deferred

- **Secret zeroization is deliberately bounded, not system-wide.** `LocalKeySigner` has one fixed-allocation canonical zeroizing secret owner (moving the signer relocates only a pointer) and constructs only operation-scoped wiping BIP-340/NIP-44 owners, including padded/decrypted plaintext and hash/cipher state; it retains no `nostr::Keys`/`SecretKey`/`Keypair`, whose pinned upstream erasure is only `non_secure_erase` (#765). NIP-46 URI/session secrets use redacted debug output and zeroizing memory, and the durable event/outbox store persists only the expected pubkey plus an opaque identity reference. This claims nothing about OS-locked memory, register erasure, or dependency-internal stack frames. NIP-46 *transport*-key copies are a separate residual owned by #766. Owner: security/signing workstream (#47).

## Protocol modules

- **NIP-65 is shipped as an engine-free Rust module plus a non-default Rust
  facade assembly (#719/#870).** `nmp-nip65` owns kind:10002 bootstrap
  composition, exact demand, canonical winner selection, marker parsing, and
  settlement without depending on `nmp`, router, store, resolver, or
  transport. `BootstrapRelayList::into_write_intent` returns an ordinary
  explicit write; `nmp::nip65::Nip65Operations` binds that value to the
  ordinary tracked receipt. The same optional feature privately turns generic
  author needs into neutral atomic route facts. **Gap:** FFI, Swift, and
  Kotlin do not yet project publication or opt-in assembly (#952/#824) and
  must not hand-roll either.

- **`nmp-blossom` covers the BUD verbs and their FFI/Swift/Kotlin projection, but not the composition layers (#545 upload, #551 mirror/delete/list, #555 projection, epic #216).** The opt-in crate ships the BUD-11 kind:24242 authorization vocabulary (draft builders for upload/delete/list — BUD-04 mirror deliberately reuses the `upload` builder — plus validate + header encoding), the BUD-02 blob-descriptor parser, and an HTTP client with the engine's admission discipline covering sha256-self-verifying `PUT /upload`, `PUT /mirror` (409/502 kept distinct, same integrity gate), single-blob-bound `DELETE /<sha256>`, and strictly parsed, bounded `GET /list/<pubkey>` (cursor/limit pagination; the deprecated `since`/`until` are not modeled). #555 projects all of it through `nmp-ffi` (`crates/nmp-ffi/src/blossom.rs`: engine-less draft/validate free functions and objects plus the blocking `FfiBlossomClient`, each operation's failure taxonomy crossing as its own typed error enum) and the hand-written SDKs (`Packages/NMP/Sources/NMP/Blossom.swift`, `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Blossom.kt`). Signing note: FFI apps sign a Blossom draft through the existing governed sign-only surface (`NmpEngine::sign_event`), which freezes the author from the ACTIVE ACCOUNT — there is no per-operation identity override on the sign-only path (the `FfiWriteIntent.identity` seam from #550/#974 covers publish intents only), so an app whose Blossom author differs from the active account must sign the draft's `unsigned_event_json` with an external/native signer and validate via `FfiBlossomAuthorization::validate`. Deliberately NOT in this unit, tracked as #216 follow-ups: the `get`/`media` endpoints (the `BlossomVerb` enum models `get` totally, but it has no draft builder), NIP-68 `imeta` picture events (T15-B), and the upload-then-publish composition seam (T15-C).

- **`nmp-nip68` owns the NIP-68 kind:20 picture-first event build + decode with imeta artifact provenance, but not the composition, projection, or richer-tag layers (#558, epic #216 T15-B-NIP68-IMETA).** The opt-in crate exclusively claims kind:20 and, engine-free and signing-free (the `nmp-nip29`/`nmp-blossom` discipline), mints a `PictureImage` artifact reference only from a content-addressed Blossom `BlobDescriptor`/`VerifiedUpload` (`url`/`m`/`x` carried by construction; a descriptor without a mime type cannot mint one), builds an immutable unsigned kind:20 draft (`build_picture`, refusing zero images), and tolerantly decodes a kind:20 event into typed picture facts with recorded diagnostics (`decode_picture`, surfacing a missing `x` as `sha256: None` + `ImetaMissingSha256` rather than trusting it). The first cut carries `title` + `imeta` images + `content-warning` + `t` hashtags only. Deliberately NOT in this unit, tracked as #216 follow-ups: the richer event-level tags (`location`/geohash, annotate-user, `L`/`l` labels); the FFI/Swift/Kotlin projection (a separate later unit); and the T15-C upload → build → sign → publish composition seam (#559).

- **`nmp-media` provides the STANDALONE staged composition seam (prepare → upload → compose) with separated failure domains, but not the durable upload, the FFI projection, or BUD-03 server-list placement (#559, epic #216 T15-C-MEDIA-COMPOSITION).** The opt-in crate wires the app-facing pipeline `Sha256Hash → signed authorization → VerifiedUpload → kind:20 draft` into three witness-typed stages so a skipped/failed stage is unrepresentable: `prepare` (holds the exact bytes it hashed and authorized — uploading those held bytes makes an authorized-hash/uploaded-bytes mismatch structurally impossible), the async standalone `PreparedUpload::upload` (a used-once obligation yielding a verified `UploadedAsset`), and `compose_picture` (the final unsigned kind:20 whose public `kind`/`tags`/`content`/`created_at` fields copy into the public-field `EventBuilder`; selecting its `pubkey` explicitly on the ordinary `WriteIntent` preserves the author through the EXISTING publish path). It defines no event schema of its own — composition is not schema ownership (`routing-and-ownership.md` §3.2.1) — and it never publishes (relay/publish is downstream). The three failure domains are three SEPARATE types (`PrepareError`, `MediaUploadError`, `MediaComposeError`) so an upload failure can never be pattern-matched or `?`-merged as a compose failure; `MediaUploadError::Blossom` preserves the whole separated Blossom `UploadError` taxonomy intact. Deliberately NOT in this unit: the upload half is NOT crash-durable (the engine-integrated durable-upload obligation — persisted intent / reattachable receipt / HTTP-publish Effect / blob persistence — is the ADDITIVE #562, whose witness types are identical to these); the FFI/Swift/Kotlin projection of the seam is a SEPARATE later unit (batched with the nip68 projection, compile-gated); and BUD-03 kind:10063 server-list placement is still deferred.

## Process / tooling

- **Required-status branch protection is not configured (#81).** Ordinary CI
  and the trusted-base `surface-governance` workflow exist, but repository
  settings must require both `surface-governance` and the protected ordinary
  `surface-regeneration` check after the governance bootstrap merges.
