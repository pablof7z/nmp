# BDD in NMP: readable contracts over the supported facade

- **Date:** 2026-07-11
- **Status:** CURRENT PRACTICE + TARGET CONTRACT. Existing untagged scenarios
  describe built behavior. `@wip` scenarios record promoted obligations whose
  public mechanism is not built.
- **Grounding:** `docs/VISION.md`, `docs/bug-class-ledger.md`, and the four
  detailed contracts under `docs/design/`.

## 1. Purpose

NMP closes a bug class only when the supported facade makes the bad path
unreachable and a falsifier proves the behavior. Gherkin scenarios are the
human-readable layer over those proofs. They do not replace crate-level unit,
property, compile-fail, persistence, or platform tests.

Scenarios must survive an internal rewrite because they speak only in public
behavior:

- an app declares a live query;
- an app publishes a write intent;
- query snapshots, receipts, diagnostics, and typed call results reveal what
  happened;
- reconstruction/restart proves what was actually durable.

The current BDD harness exercises the real Rust runtime against scripted local
relays. Swift and Kotlin falsifiers remain necessary where the native reactive
or secure-provider boundary is part of the contract.

## 2. Vocabulary and bias control

Scenario prose uses people, protocol operations, query meaning, planned
sources, receipt facts, and diagnostics. It avoids Rust implementation names.

The initial executable fixtures happen to contain kind:1 notes and NIP-02
contact lists because that is what the existing harness can build. They are one
protocol-shaped exemplar, not the BDD ontology and not a preferred NMP product.
New acceptance groups must include kind-diverse and module-composition cases.

Use these distinctions:

| Concept | Scenario wording |
|---|---|
| Reactive input | current pubkey, or a named app input once that target exists |
| Signer choice | default identity for this publish, or explicit identity override |
| Local data | cached/matching rows in this engine |
| Acquisition | planned source, requested, AUTH-blocked, EOSE observed, unavailable, limited |
| Relay context | typed indexer policy or protocol host context, never an arbitrary route list |
| Protocol ownership | exact NIP-defined schema/operation, not a broad content category |
| Durability | accepted obligation, pending row, signer waiting, attempt, ACK/rejection |

Do not write `synced`, `syncHealth`, `globally complete`, `authoritative empty`,
or "the cache is truth." EOSE and watermarks are source/request facts.

Do not use "account leak" to describe valid rows shared inside one engine. One
engine is one local trust domain. Test dependency-scoped rerooting and explicit
destructive reset instead.

## 3. Admissible observables

Every `Then` resolves to an app-visible surface:

1. **Query snapshots:** current canonical local rows plus cache, acquisition,
   and shortfall evidence.
2. **Receipt facts:** acceptance, signer waiting, signature promotion, route and
   attempt facts, per-relay outcomes, cancellation, expiry, and ambiguity.
3. **Diagnostics:** exact plan revision, wire filters, connection/AUTH/EOSE,
   event counts, lanes, coalescing, limits, pressure, retry, and errors.
4. **Typed operation results:** rejection before acceptance, contextual
   composition failure, destructive reset completion, or provider attach
   result.
5. **Restart observation:** reconstruct the engine and assert the same public
   query/receipt/diagnostic facts, never inspect database internals directly.

Current `Coverage.unknown | completeUpTo` steps are allowed only in executable
current scenarios and must be described as the current aggregate API. Target
scenarios use per-planned-source evidence.

Timing belongs to bounded test helpers, not prose. Production behavior contains
no sleep-and-check polling.

## 4. Tooling and suite structure

The suite uses `cucumber` with plain `.feature` files and one fresh `NmpWorld`
per scenario. It runs the real `EngineThread` against scripted in-process
relays. Existing crate-level mechanism tests remain in place.

```text
features/
  queries/
  routing/
  identity/
  writes/
  coverage/
  sync/
  diagnostics/
  modules/       # target protocol ownership/composition
  limits/        # target boundedness/shortfall
  must-never/
crates/nmp-bdd/
  src/world.rs
  src/relays.rs
  src/steps/{given,when,then}.rs
  tests/bdd.rs
```

Tags carry meaning:

- `@ledger-N`: maps the scenario to bug-class ledger entry N.
- `@must-never`: stages a forbidden runtime consequence.
- `@wip`: promoted target behavior that the runner intentionally excludes
  until its owning implementation issue lands.
- `@live`: bounded real-network proof, excluded from the default deterministic
  suite.

The step catalog is closed and reviewed. A new behavior may add a reusable step
in the same implementation PR; scenarios do not invent ad hoc synonyms.

## 5. Current executable scope

The current harness proves a narrow but real slice:

- a NIP-02-derived author set reroutes surgically when the contact list changes;
- indexers bootstrap author write-relay discovery;
- current `Reactive(ActivePubkey)` demand reroots;
- receipts report divergent per-relay ACK/rejection;
- the current aggregate unknown state is distinct from an empty row set;
- capped routing, NIP-77 capability gating, reconnect replay, and diagnostics
  have executable scenarios.

These remain current implementation evidence. They must not be generalized in
prose beyond what their assertions prove.

## 6. Promoted target scenario groups

### Query demand and evidence (`#7`, `#11`, `#18`)

- Changing `$currentPubkey` reroots only dependent observations while a literal
  multi-account query remains live.
- Equal selections under different source/AUTH contexts do not borrow evidence.
- One source at EOSE plus one offline/AUTH-blocked source yields both facts and
  no global completion state.
- A reusable NIP-02 fragment prints the same closed graph as raw construction.
- Engine-imposed shortfall is distinct from a caller-requested result bound.

### Durable write, signer, and retry (`#9`, `#10`, `#15`, `#16`, `#19`)

- `Accepted` survives immediate process death with the pending row and receipt.
- Matching ordinary and derived queries see the unsigned pending row through
  normal store semantics.
- Default signer selection and explicit override are pinned at acceptance.
- Missing NIP-46/provider capability remains durable `AwaitingSigner` and
  resumes after a matching provider attaches.
- Invalid signer responses cannot promote the row.
- Pre-signature cancellation restores a displaced replaceable winner.
- Relay rejection after signing changes receipt facts only.
- Retry ordinal and next eligibility survive restart; at-most-once ambiguity
  never sends twice.

### Protocol modules and composition (`#3`, `#6`, `#14`)

- A module claims only exact schemas defined by its NIP.
- NIP-29 adds `h` and group-host context to a foreign-owned immutable draft
  without claiming its kind.
- Core validates the final body, signs once, and exposes the contextual route in
  diagnostics.
- Enabling no protocol module retains a useful raw two-noun engine.
- Swift, Kotlin, and direct Rust produce byte-identical unsigned bodies for the
  same composed operation.

### Bounded delivery and reset (`#4`, `#17`)

- Slow query/diagnostic observers have bounded memory and eventually see the
  newest exact local state.
- Receipt observers may detach and reattach without losing persisted facts.
- Oversized derived demand chunks exactly or reports shortfall; never first-N.
- Ingress overload backpressures or disconnects with a diagnostic reason.
- Explicit destructive reset clears cache, pending writes, receipts, evidence,
  and capabilities before another untrusted local user enters.

The corresponding `@wip` feature files are durable acceptance targets, not
claims of current build status.

## 7. Scenario style

- One promise per scenario title.
- Stage topology, protocol facts, app state, then one triggering action.
- Assert only through the admissible public observables.
- Use content kinds only where the protocol scenario needs them; do not make
  kind:1 the default placeholder for unrelated architecture.
- A cap scenario must allow explicit shortfall when its objective cannot be
  satisfied. Never assert "at least two" and "under cap" as simultaneously
  guaranteed for impossible inputs.
- A current scenario must pass before merge. A target scenario stays `@wip`
  until its implementation and supported-facade falsifier land together.
- When behavior changes, update the scenario, ledger, canonical design doc,
  platform projections, and builder guidance in the same governed change.

## 8. Completion discipline

Removing `@wip` is a proof promotion. The owning PR must identify:

1. the structural mechanism that excludes the bug;
2. the deterministic BDD scenario;
3. lower-level mechanism tests;
4. restart tests where durability is claimed;
5. diagnostics assertions for invisible routing/evidence/retry behavior; and
6. native Swift/Kotlin falsification where the platform boundary participates.

Passing prose is not proof. Passing one platform is not a cross-platform
contract. A public behavior becomes `BUILT` only when the supported facade and
required projections agree.
