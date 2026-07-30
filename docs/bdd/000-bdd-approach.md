# Behavioral specifications and executable evidence

- **Date:** 2026-07-30
- **Status:** CURRENT MODEL + INCREMENTAL MIGRATION
- **Owner:** #1071
- **Grounding:** `docs/VISION.md`, `docs/bug-class-ledger.md`, and
  `docs/design/architecture-review-gates.md`

## 1. One claim, one identity, one proof owner

Readable behavior and executable evidence are different artifacts:

- English states the externally meaningful distinction.
- An evidence locator names the executable witness.
- A falsifier says what must turn red if the mechanism disappears.

No directory, tag, feature name, or passing adjacent test implies that a
behavior is built. A `built` claim exists only when all three artifacts agree.
This follows the ledger's closing rule: a bad path is excluded by a structural
mechanism plus a falsifier, never by prose or reviewer memory.

Each scenario has one stable identity. That identity follows the behavior when
its English specification or executable proof moves to its semantic owner. Do
not copy a scenario into another runner, mint a replacement ID for the same
distinction, or preserve the old spelling as a compatibility alias.

## 2. Current migration boundary

The root `features/` tree and `crates/nmp-bdd` predate this model. #1071's
source-traced audit records all 335 legacy scenario definitions before any
bulk restructuring:

- the root corpus has lifecycle encoded by `@wip`, `@designed`, or the absence
  of a tag;
- those meanings disagree between old prose and the runner;
- the runner uses `nmp/unstable-mechanism`, fixture routing facts, and raw
  mechanism handles;
- a green `nmp-bdd` run is therefore mechanism integration evidence, not proof
  that the supported application facade works.

Migration is physical and owner-by-owner:

```text
features/                                  # audited legacy corpus; shrinking
crates/<owner>/tests/behavior/**/*.feature # governed English at mechanism owner
crates/nmp/tests/acceptance/**/*.feature   # governed public-facade capstones
```

Every `.feature` file below a governed owner path is validated by
`scripts/check-behavior-traceability.py`. The legacy root is not placed on an
allowlist and there is no second manifest. A coherent PR moves or deletes the
old scenario and adds governed metadata at the new owner in the same change.
When the final legacy behavior has an owner, the separate `nmp-bdd` crate and
its BDD-only architecture gate are deleted.

New behavior must not be added to the legacy root during migration.

## 3. Status is explicit metadata

Machine-readable comments sit contiguously above every governed `Scenario` or
`Scenario Outline`. Tags may follow the metadata block. Gherkin tags attached
to a `Feature` or `Rule` are inherited by every scenario in that scope, so the
validator applies the same lifecycle and acceptance rules to the effective
union of Feature, Rule, and Scenario tags.

```gherkin
# nmp:id=ROUTING-DISCOVERY-003
# nmp:status=built
# nmp:evidence=rust:nmp::public_engine_bootstraps_author_route_before_content_fetch
# nmp:falsifier=disabling NIP-65 ingestion leaves the final public query empty
@acceptance
Scenario: A cold feed discovers the author's route before fetching content
```

The ID format is `<DOMAIN>-<CONTEXT>-<NNN>`. IDs are never renumbered or
recycled. A behavior merged into another behavior retains its history in the
owning issue and git rather than acquiring a second live identity.

Exactly three statuses exist:

### `specified`

The behavior is agreed, but its implementation or required evidence is not
complete.

Required:

```gherkin
# nmp:status=specified
# nmp:gap=implementation
# nmp:issue=#123
```

`nmp:gap` is exactly one of `implementation`, `evidence`, `fixture`, or
`platform`. The issue must be open and must explain the consequence of the
gap. A specified scenario may point to narrower evidence, but that evidence
does not promote the broader claim.

### `built`

The behavior is implemented at the claimed boundary and its falsifier has
been performed.

Required:

```gherkin
# nmp:status=built
# nmp:evidence=rust:nmp-router::coverage_respects_whole_demand_cap
# nmp:falsifier=changing cap-exhausted shortfall to no-candidates makes the owner test fail
```

The evidence locator format is `<kind>:<owner>::<test-or-check>`. Current kinds
are `rust`, `property`, `compile`, `script`, `swift`, and `kotlin`. A locator
names the narrowest executable owner; it is not a link to a plan, issue,
comment, or prose-only document.

### `known-violation`

The scenario deliberately records behavior that is currently false.

Required:

```gherkin
# nmp:status=known-violation
# nmp:issue=#456
```

The open issue owns the repair. Do not make a false scenario green, exclude it
through a runner expression, or relabel it as a capability.

## 4. Tags do not carry lifecycle

`@acceptance` is the only execution-selector tag in the governed deterministic
corpus. It means that a **built** scenario is one of the few cross-component
journeys executed through the supported public facade.

The validator rejects:

- `@wip`;
- `@designed`;
- `@live`; and
- any `@requires-*` tag.

Feature flags, environment variables, protocol availability, and platform
applicability are not lifecycle states. A platform-specific contract is
represented by a platform evidence locator or an explicit `platform` gap.
Real-network smoke tests live in a separate bounded workflow and can never be
the sole evidence for a correctness claim.

`@ledger-N` and `@must-never` remain explanatory mappings. They do not promote
status.

## 5. Put proof at the semantic owner

Choose evidence by the behavior's boundary, not by the historical test
directory:

| Claim | Canonical evidence |
|---|---|
| Supported Rust application journey | Public-facade acceptance target using `Engine::new` and public observations |
| Reducer/state transition | Owning crate unit or integration test |
| Routing invariant across many inputs | Property/model test with a differential oracle |
| Durable write, receipt, or evidence | Store/runtime restart or crash test |
| Forbidden dependency/API construction | Compile-fail test or trusted script |
| Swift/Kotlin reactive or provider behavior | XCTest/Gradle test at that platform boundary |
| Real relay compatibility | Bounded smoke test plus deterministic evidence elsewhere |

An English `.feature` beside a mechanism owner is not automatically a
Cucumber target. It preserves the distinction and points to the owner-native
proof. Only `@acceptance` scenarios belong to the public-facade runner.

Do not duplicate mechanism tests through Cucumber step definitions merely to
make the English executable. Conversely, do not cite a lower-level reducer
test as acceptance evidence for a public-facade journey.

## 6. Facade acceptance must be independently observable

A public-facade capstone:

1. constructs the supported `Engine` through `Engine::new`;
2. uses only public query/write verbs;
3. observes public rows, receipts, diagnostics, typed results, or reconstructed
   state;
4. drives independent scripted relays through their wire boundary;
5. does not inject the answer through `FixtureRoutingFacts`, raw reducer
   commands, a mechanism handle, or a shared private state object; and
6. fails if the named production behavior is removed.

For example, a discovery bootstrap test must seed a real kind:10002 only at an
indexer, start without the target author's route, independently witness the
discovery request, and observe content only after ingestion. Preloading the
author route can prove a no-fallback routing rule, but it cannot prove
discovery.

## 7. Falsifier workflow

Before marking a scenario `built`:

1. identify the production decision that owns the behavior;
2. name the exact evidence test/check;
3. make the smallest meaningful mutation that removes or corrupts that
   decision;
4. run the named evidence and record its relevant red failure;
5. restore production code;
6. rerun the evidence green; and
7. report the mutation and both results on the owning issue/PR.

Changing only the test assertion is weaker than mutating the mechanism and is
not sufficient when a production seam can be changed safely. Timing out a
test is evidence only when the missing behavior is itself bounded liveness and
the bound has enough margin to distinguish it from a slow machine.

## 8. CI and local commands

The traceability lane is intentionally separate from workspace compilation:

```bash
python3 scripts/test-behavior-traceability.py
python3 scripts/check-behavior-traceability.py
```

The first command falsifies the validator itself. The second checks every
governed behavior file, resolves Rust/property evidence into its owner crate,
and rejects duplicate or incomplete claims.

Owner tests remain separate commands, for example:

```bash
cargo test -p nmp-router --test contract coverage_respects_whole_demand_cap
```

The public-facade acceptance target and its distinct CI lane land when the
first genuine capstone migrates. Native evidence remains in the existing
macOS/Kotlin qualification lanes.

## 9. Change and review discipline

A behavioral PR must answer:

- What user-visible or architectural distinction changed?
- Which stable IDs are added, moved, merged, or deleted?
- Is product status independent from proof status?
- Does every built locator resolve to the exact owner test/check?
- What production mutation was performed, and what failed?
- Does the fixture establish the condition independently, or inject the
  answer?
- Can the proof pass while the public behavior named in English is broken?
- Is the scenario at the semantic owner, with the old duplicate removed?
- Are documentation, known gaps, SDK projections, and native evidence updated
  where the boundary requires them?

No PR may promote status by merely removing a tag. No execution plan or issue
comment supersedes this current owning document; when the model changes, this
document changes with it.
