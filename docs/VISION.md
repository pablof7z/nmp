# NMP v2 - vision and validation frame

- **Date:** 2026-07-11
- **Status:** Architecture frame promoted after the M0-M5 exploration. Behavioral
  invariants are settled enough to direct work; public names and shapes remain
  provisional until v2.
- **Detailed contracts:** `docs/design/query-demand-and-evidence.md`,
  `docs/design/durable-write-signing-and-retry.md`,
  `docs/design/protocol-modules-and-composition.md`, and
  `docs/design/bounded-delivery.md`.
- **Review enforcement:** `docs/design/architecture-review-gates.md` turns the
  two-noun rule below (and the bug-class-ledger's type-over-convention rule)
  into a PR-review checklist, so a third public noun or an unenforced
  precondition is caught at review time instead of by the next architectural
  sweep.

## 1. Thesis

**NMP is an embeddable Nostr sync-and-routing engine: a library an app talks to,
not a framework an app lives inside.**

NMP owns the Nostr machinery that unrelated apps otherwise reproduce badly:
event storage, replaceable/delete/expiry semantics, provenance-preserving dedup,
query dependency resolution, relay routing, subscription coalescing, sync,
durable publication, and diagnostics. The app keeps its architecture, state,
navigation, presentation, account UX, and product policy.

The app-facing model has two nouns:

1. **Live query.** A closed, introspectable demand value observed through the
   platform's native reactive primitive.
2. **Write intent.** A durable or explicitly non-durable publication obligation
   observed through a reattachable receipt.

Diagnostics are a permanent proof surface over both nouns, not a third command
surface or optional debug mode.

## 2. Live-query contract

### 2.1 Demand is more than a filter

The semantic descriptor is:

```text
Demand := Selection + SourceAuthority + AccessContext
```

- **Selection** says which stored events match. It is a Nostr `Filter` whose
  field values may be reactive bindings.
- **Source authority** says which relay facts and routing policy are allowed to
  acquire the selection.
- **Access context** carries protocol context that can affect what a relay is
  asked for or allowed to return, such as AUTH state. It is not a local
  account-privacy boundary.

All three participate in demand identity, routing, coverage evidence, and safe
wire coalescing. Identical selection subgraphs may share internally, but NMP
must not conflate acquisition evidence from different authorities or access
contexts.

Two per-observation policy axes sit beside that semantic identity:

- **Cache mode** controls which matching local rows are projected, including
  strict provenance filtering for pinned sources.
- **Freshness** is `Live` (cache then maintain live wire work),
  `MaxAge(seconds)` (suppress this handle's wire contribution when every
  currently assigned source has sufficiently recent coverage), or `CacheOnly`
  (never contribute wire work).

Neither axis changes atom, wire, or coverage identity. Equal observations can
share the same graph and coverage history while retaining independent local-row
and wire-lifetime contracts. Freshness is decided once when a handle opens; it
does not create a polling loop or a third app-facing noun.

### 2.2 Reactive binding grammar

```text
Binding  := Literal(set)
          | Reactive(CurrentPubkey)
          | Derived(inner: Filter, project: Selector)
          | SetOp(Union | Intersect | Diff, [Binding])
Selector := Authors | Ids | Tag(name: String) | AddressCoord
```

`Tag`'s `name` is an arbitrary event-tag key, not restricted to a single
letter — it projects already-acquired events locally, so it carries no wire
syntax restriction (distinct from a wire/local `Filter`'s indexed tag keys,
which stay exactly NIP-01's single-ASCII-letter alphabet). Selectors remain
closed, typed, hashable, and introspectable. App closures
never enter demand, admission, routing, ordering, or cursor decisions.

Reference selectors preserve routing evidence with the value they project:
`Tag("e")`, `Tag("a")`, and `Tag("p")` use a valid tag relay hint when one is
present and otherwise retain the source event's observed-relay provenance;
`AddressCoord` retains source provenance. Evidence follows the value through
interior `Derived` and `SetOp` nodes, is admission-gated before routing, and
does not change which events the resulting filter matches.

A graph's derived depth counts only edges that enter `Binding::Derived`.
`SetOp` is a same-level value combinator and does not consume another Derived
hop, however deeply its operands are nested syntactically.

`$currentPubkey` is a useful reactive root, not a global engine identity. When
the app changes it, only graphs that reference it re-resolve. A simultaneous
literal query such as `#p:[accountA, accountB]` remains live and unchanged.

Apps and opt-in protocol crates may publish reusable **derived fragments** that
construct this grammar. A follows fragment, for example, expands to an ordinary
`Derived` graph; it is not an opaque macro or a privileged core concept. Richer
protocol helpers may return typed values, such as group references containing a
group id and host relay, while their underlying demand remains inspectable. NIP-29's
own remembered-group browsing (#108) is exactly this shape: `nmp-nip51` owns and
decodes NIP-51 kind:10009, and `nmp-nip29` composes the result into typed group
references, never a privileged core concept.

The core must not privilege kind:1, timelines, follows, or any other content
shape. Proofs and examples must remain kind-diverse.

### 2.3 Rows plus evidence, never global truth

A query snapshot contains:

- the current matching cache rows;
- compact cache/acquisition evidence scoped to the query's current source plan;
- explicit local shortfall or limit evidence when applicable.

NMP never reports `synced`, `syncHealth`, global `complete`, or an
"authoritative empty" interpretation. EOSE, negentropy reconciliation, cached
watermarks, connection failures, and AUTH challenges are facts about particular
planned sources. They cannot prove that no unknown or private relay has more
matching events.

Most apps need only the compact snapshot evidence. Diagnostics retain the raw
per-relay plan, exact wire filters, connection/AUTH state, EOSE/watermarks,
events received, and routing explanation.

## 3. Write-intent contract

### 3.1 `Accepted` is a durable fact

For a durable intent, `Accepted` means one crash-consistent logical acceptance
boundary contains:

- the frozen unsigned event body and expected author;
- the intent and receipt state;
- a canonical pending event row visible through ordinary matching queries.

The current Redb backend realizes that boundary with one physical transaction,
but the guarantee does not require every authority domain to share a database.
A future split may commit the complete publishing obligation to an authoritative
control store first, then project the pending row through a deterministic,
idempotent journal before normal queries or transport resume. The reverse order
is forbidden. Canonical rows and all event-local indexes, tombstones,
replacement state, and expiry state remain atomic inside the event store. The
settled invariant and relay-echo reconciliation rule are recorded in
`docs/design-record.md`.

NIP-01 event identity does not include the signature, so the pending row has a
stable id. Its signature state is typed:

```text
Pending(intentId) | Signed(signature)
```

The write path never pushes rows to observers. Acceptance, signature promotion,
cancellation, deletion, replacement, and expiry all mutate the canonical store;
ordinary query invalidation makes the change visible everywhere it matches.

If an unsigned replaceable row temporarily displaces another row, cancellation
or terminal pre-signature failure performs a compensating store mutation that
restores the prior winner. Once signed, relay rejection changes receipt evidence
only; it does not retract a valid signed event from the cache.

### 3.2 Signer selection

The common API does not make apps carry signer objects through every call:

```text
publish(draft)                    // signer for currentPubkey
publish(draft, as: identityRef)   // explicit override
```

NMP resolves the default signer registered for `$currentPubkey`. An override is
for podcast identities, disposable keys, delegated identities, and similar
cases. The selected author/signer identity is frozen at `Accepted`; later
changes to `$currentPubkey` cannot redirect an outstanding intent.

When the signer is unavailable, the durable intent becomes
`AwaitingSigner(pubkey)` and remains pending until a matching signer is attached
or the app explicitly cancels it. NMP persists the obligation, not raw secret
material. Platform SDKs should provide standard secure signer providers backed
by platform facilities; apps own identity import/removal/backup UX and may
supply custom remote or hardware providers.

### 3.3 Durable delivery and retry ownership

A durable intent remains live until explicit cancellation, a terminal
signer/protocol failure, protocol expiry, or the required per-relay outcomes are
recorded. Temporary signer, network, relay, or AUTH unavailability never
silently closes it.

There is one retry owner per domain:

- transport owns socket reconnection only;
- a remote-signer adapter owns one correlated signer operation;
- the durable outbox owns persisted `(intent, relay)` attempts;
- one engine deadline scheduler owns timers and concurrency limits.

Durable event bytes are persisted before send. Transport must not hide a second
durable EVENT queue. Attempts record their ordinal, start, outcome, and next
eligible time. Offline or AUTH-blocked time does not consume attempts; transient
failure advances logical backoff; recovery wakes eligible work. At-most-once
ambiguity becomes `OutcomeUnknown`, never a blind resend.

## 4. Protocol modules and composition

The engine core is content-agnostic but protocol-aware at its lowest universal
layers. Opt-in NIP/feature crates provide protocol schemas, validation, codecs,
state reconstruction, derived query fragments, and semantic operations without
turning the core into a content catalog.

A module owns only the exact event schemas defined by its protocol. NIP-29, for
example, owns its group metadata/admin/membership/moderation events; it does not
own an article, photo, podcast, or other foreign content kind merely because
that content is published into a group.

Composition uses immutable unsigned drafts:

```text
blob  = Blossom.upload(file)
photo = Nip68.buildPhoto(blob)
receipt = group.publish(photo)
```

The schema module builds the draft. A contextual module adds only its
protocol-defined contribution, such as NIP-29's `h` tag and host-relay
constraint. The core validates the combined value, signs once, and publishes.
Blossom upload failure and Nostr publication failure remain separate outcomes.
No app closure or module registration callback enters the decision path.

## 5. Cache, accounts, and local trust

One engine instance is one local trust domain. Verified public events, pending
rows, provenance, and matching query results share one canonical cache across
identities. Account selection is not a cache-visibility boundary.

Access/AUTH context still participates in acquisition evidence because a relay
may answer differently under different authentication. Those events merge into
the common store after validation; their source evidence remains attributable.

Apps that serve mutually untrusted local users must use an explicit destructive
reset/logout operation that clears cached events, pending writes, receipts,
coverage/evidence, and related local state. Silent partial cleanup is unsafe.

## 6. Boundedness and backpressure

- Query and diagnostic observations are latest-state streams. A slow observer
  may skip intermediate snapshots but eventually receives the newest complete
  local state.
- Receipt transitions are durable facts and are reattachable; they are not an
  unbounded in-memory observer queue.
- Large derived sets may be chunked only when chunking preserves semantics and
  the complete demand remains explainable. Projected id atoms are packed into
  deterministic widen-only wire filters of at most 256 ids; further ids ship
  as additional exact chunks rather than being truncated.
- When graph, wire, relay, or result limits prevent the full planned acquisition,
  NMP still returns cached rows with explicit local shortfall evidence.
- NMP never silently takes the first N values and presents them as the complete
  result.
- Ingestion is backpressured. An overwhelming relay may be disconnected, with
  the exact reason retained in diagnostics.

See `docs/design/bounded-delivery.md` for the contract.

## 7. Ownership boundary

| NMP owns | The app owns | The UI framework owns |
|---|---|---|
| Canonical event store, persistence, provenance, replace/delete/expiry semantics | App state and architecture | Rendering and layout |
| Demand resolution, routing, REQ lifecycle, coalescing, source evidence | Which queries exist and their app-controlled inputs | Observation scope |
| Outbox, signer orchestration, durable receipts, retry scheduling | Account list and UX, current-pubkey input, optional signer override | Navigation and presentation lifecycle |
| Standard platform signer-provider integrations | Key import/removal/backup policy and custom providers | Platform secure-storage implementation |
| Protocol-neutral core and opt-in protocol-module seams | Which protocol features are enabled | Formatting and display policy |
| Diagnostics and destructive engine reset | When reset/logout is requested | User-facing reset confirmation |

The supported app assembly path is one invariant-preserving Rust facade used by
direct Rust consumers and by FFI. Swift and Kotlin SDKs project that facade into
native reactive types. Mechanism crates are implementation units, not alternate
ways for apps to assemble a partially-correct engine.

This ownership table does not require each app to rebuild the open-ended Nostr
content renderer. Optional content/UI packages may consume the same public
facade to provide reusable parsing, scoped reference sessions, native
primitives, and styled source-installable components. They remain replaceable
consumers above the engine, never a third engine noun or a source of routing,
store, navigation, or product-policy truth. See
`docs/design/ui-components-strategy.md`.

## 8. Public-surface governance

Behavioral invariants above are the frame. Public names, enum cases, FFI records,
and method signatures are not frozen before v2.

A public-shape change is allowed only when it:

1. records what the current shape cannot express or makes unsafe;
2. assesses Rust facade, persistence, diagnostics, FFI, Swift, and Kotlin impact;
3. updates affected projections and falsifiers together;
4. receives explicit human signoff; and
5. removes the superseded path instead of maintaining premature parallel APIs.

The project freezes an earned cross-platform contract after falsification; it
does not freeze a guess. Until then, compatibility is intentionally provisional
and changes are deliberate rather than casual.

## 9. Validation frame and work order

The original milestone gates established the thesis in stages: grammar,
compiler/router, store/transport/outbox, Swift boundary, an iOS falsifier, then a
second-platform proof. Those implementation verdicts remain recorded in reviews
and GitHub Issues; they do not make later architectural corrections retroactive.

The minimal JVM Kotlin/Flow projection and live-relay falsifier landed in #54.
It proves the reactive dialect and deterministic cancellation shape, not the
full Android/AAR/Compose milestone or parity with the promoted target contract.

The next work is ordered by dependency:

1. Govern the provisional demand, snapshot, intent, signer, module, and receipt
   shapes across the Rust facade and platform projections.
2. Build crash-safe `Accepted`, pending rows, signature promotion,
   cancellation/compensation, and reattachable receipts.
3. Build signer reattachment and standard platform provider boundaries.
4. Make source/access-context routing and protocol-module composition exact.
5. Enforce bounded delivery and explicit shortfall throughout the engine.
6. Falsify the same invariants through direct Rust, Swift, Kotlin, restart,
   overload, signer-offline, and protocol-module scenarios.

GitHub Issues are the tactical queue. `docs/known-gaps.md` states what remains
unbuilt; `docs/bug-class-ledger.md` claims only mechanisms with real proof.
