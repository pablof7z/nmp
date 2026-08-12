# BDD in NMP: readable contracts over the supported facade

- **Date:** 2026-07-11
- **Status:** CURRENT PRACTICE + TARGET CONTRACT. Governed scenarios carry
  explicit `nmp:status`, evidence, and falsifier/issue metadata. Unchanged
  ungoverned files are temporary migration debt, not implicit built claims.
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

The transitional BDD harness exercises the real Rust runtime against scripted
local relays. It is mechanism evidence, not facade acceptance. Swift and Kotlin
falsifiers remain necessary where the native reactive or secure-provider
boundary is part of the contract.

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

## 4. Canonical corpus, governance, and transitional execution

Canonical English lives only under `features/<behavioral-domain>/`. Executable
proof stays with the narrow contract owner and is connected by metadata; crate
structure never owns or duplicates the English specification.

`tools/behavior-traceability` parses every feature through the Gherkin 0.14 AST.
It uses AST hierarchy, positions, and spans for Feature/Rule/Scenario meaning,
and reads only the adjacent `# nmp:*` comment block from source. Indentation and
whitespace after `#` do not change that lexical boundary; the transitional
runner uses the same conservative whole-file sentinel. It validates
status, evidence, issues, inherited tags, and explicit base/head diffs without
depending on `nmp-bdd`, so governance survives that runner's retirement.
The checker is a detached Cargo workspace with its own lockfile and explicit
targets. CI builds it from a neutral temporary directory and Cargo home; root
workspace dependencies and repository Cargo configuration cannot enter its
executable graph. A protected credentialed step resolves only the exact
deduplicated issue-number/state snapshot, then the head-built checker runs
without a GitHub token. Live issue numbers and states are never mirrored in
checker source or unit-test fixtures: unit tests use synthetic snapshots, while
only the credential-free injected CI snapshot validates the canonical corpus.
Rust locators do not trust source-file syntax. After that credential boundary,
the checker runs pinned Cargo metadata and `cargo test --no-run` from a neutral
directory with isolated Cargo home/target state, exact workspace package IDs,
bounded process groups, and bounded output. It accepts only unique,
non-ignored names reported by normal libtest harnesses; dead or feature-gated
files, other packages, custom harnesses, failed builds/lists, and ambiguous
names cannot become evidence. Build scripts, proc macros, and harness
initialization therefore run only inside this credential-free bounded phase.
Executable evidence maps only to a closed proof-step grammar: one whole `run`
command on a known hosted-runner family, under the runner's ordinary Bash, that
names its proof tool directly — `cargo`, `swift`, the repository-owned Gradle
wrapper, or the exact repository script path. Setup steps, wrappers, shell
control flow, environment prefixes, substitutions, backgrounding, and any
trailing command are not proof: the proof must be the terminal command in its
shell context, and a step that disables errexit or follows the proof with
anything else is rejected. A non-Bash `shell:` carries no lane claim at all.

This grammar decides *whether a required lane runs the named proof*, not
whether that lane's shell can be subverted from inside the workflow that owns
it. `.github/workflows/ci.yml` is an owner-protected path of the
surface-governance migration policy, so a pull request cannot introduce a
command shadow there without owner authorization; that protection, not this
grammar, is what keeps the mapped lane honest.

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
  src/world/       # {budgets,queries,observe,staging,actions,watches}.rs
  src/relays.rs
  src/steps/{given,when}.rs
  src/steps/then/  # {feed,receipts,routing,wire,budget}.rs
  tests/bdd.rs
tools/behavior-traceability/
  Cargo.lock        # detached checker dependency closure
  src/              # AST, metadata, locators, issue state, explicit git diff
```

Every file in `crates/nmp-bdd` stays under 600 lines
(`scripts/check-bdd-file-length.sh`); each module's own doc comment says what
it owns and why that is the boundary.

Every governed scenario has:

- one stable `nmp:id`;
- exactly one `nmp:status=built|specified|known-violation`;
- ordered, repeatable, distinct evidence plus one falsifier when built;
- a typed gap and open issue when specified;
- an open issue when known-violation.

Once any scenario in a file has `nmp:*` metadata, every scenario in that file
is governed. Added, changed, moved, or deleted behavior must be governed;
unchanged legacy files may remain temporarily. An ungoverned legacy file must
first become governed in a traceable change before a later deletion. A governed
file rejects `@wip`, `@designed`, and `@requires-*` inherited through Feature,
Rule, Scenario, or Examples tags. `@ledger-N` and `@must-never` retain their
behavioral meaning; `@acceptance` is a built facade-capstone selector, not
lifecycle state.

The transitional `nmp-bdd` runner skips every scenario in a governed file.
Ungoverned legacy retains its old `@wip`/`@designed`/`@live` filter while
migration proceeds. #1077 owns the one supported-facade acceptance target;
this mechanism runner never impersonates it.

The `nmp` acceptance target under `crates/nmp/tests/acceptance/` is the one
deterministic public-facade Cucumber runner. It loads selected canonical
`features/<domain>/` files directly, constructs `Engine::new`, and uses
independent scripted-relay witnesses; no `.feature` copy lives beside the
test. CI runs it in the distinct `facade-acceptance` job.

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
- Missing signer capability remains durable `AwaitingSigner` and
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

These targets use `nmp:status=specified`, a typed gap, and an open issue until
their implementation and evidence are ready. They are not current build
claims.

## 7. Scenario style

- One promise per scenario title.
- Stage topology, protocol facts, app state, then one triggering action.
- Assert only through the admissible public observables.
- Use content kinds only where the protocol scenario needs them; do not make
  kind:1 the default placeholder for unrelated architecture.
- A cap scenario must allow explicit shortfall when its objective cannot be
  satisfied. Never assert "at least two" and "under cap" as simultaneously
  guaranteed for impossible inputs.
- A built scenario's evidence must pass before merge and fail under its named
  product falsifier. A target stays `specified` with an open issue until its
  implementation and sufficient evidence land together.
- When behavior changes, update the scenario, ledger, canonical design doc,
  platform projections, and builder guidance in the same governed change.

## 8. Completion discipline

Changing `nmp:status=specified|known-violation` to `built` is a proof promotion.
The owning PR must identify:

1. the structural mechanism that excludes the bug;
2. the deterministic BDD scenario;
3. lower-level mechanism tests;
4. restart tests where durability is claimed;
5. diagnostics assertions for invisible routing/evidence/retry behavior; and
6. native Swift/Kotlin falsification where the platform boundary participates.

Passing prose is not proof. Passing one platform is not a cross-platform
contract. A public behavior becomes `built` only when its mapped evidence and
required projections agree. `@acceptance` additionally requires a
`rust:nmp::<test>` facade proof and will execute only in the #1077 target.
