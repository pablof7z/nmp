# Test placement

Place executable proof with the narrowest stable contract that owns the guarantee.

Do not place tests by whichever crate was edited. Do not put copies of one product scenario in every crate that participates.

## Two different maps

NMP has two valid decompositions:

- **Feature files:** organized by behavioral domain and user-visible meaning.
- **Executable tests:** organized by contract ownership and proof technique.

A routing scenario can span resolver, router, store, transport, protocol modules, and the engine while its primary structural proof belongs in `nmp-router` and its public capstone belongs in `crates/nmp/tests`.

## Placement decision tree

### 1. Is the claim local to a value, parser, codec, or pure transition?

Place it in the owning crate as a unit or table-driven test.

Examples:

- canonical filter normalization;
- event schema decode;
- signer response validation;
- receipt state transition.

### 2. Is the claim an invariant over many inputs or operation sequences?

Place a property, model, or differential test in the owning mechanism crate.

Examples:

- routing never selects an inadmissible destination;
- dependency-scoped rerooting leaves unrelated demand unchanged;
- replaceable-event winner selection is order-independent under allowed histories;
- bounded planning either returns the exact set or an explicit shortfall.

Do not enumerate this state space as dozens of Gherkin examples.

### 3. Does the claim require the crate's public surface plus real collaborators?

Use a crate integration test in that crate's `tests/` directory.

Examples:

- `nmp-store` closes and reopens a durable receipt;
- `nmp-transport` reconnects and replays a request;
- `nmp-signer` resumes an awaiting-signature obligation after provider attach.

### 4. Does the product facade own the combined promise?

Place the proof under `crates/nmp/tests/` and drive `nmp` as a consumer would.

Examples:

- a query discovers an author's relay and yields content;
- an active-account change reroutes only dependent subscriptions;
- accepted work survives process reconstruction;
- receipt frames truthfully expose per-relay divergence.

This layer may assemble deterministic collaborators through `nmp-test-support`, but actions and observations remain facade-owned.

### 5. Is the claim a high-value, cross-layer capstone whose English example improves understanding?

Represent it in the feature corpus and, when appropriate, tag it `@acceptance`.

The acceptance runner should normally live as a custom integration target under `crates/nmp/tests/acceptance/`.

A separate acceptance crate is justified only when it behaves like a real downstream consumer. It should depend primarily on:

- `nmp`;
- `nmp-test-support`;
- generic test libraries;
- external protocol/relay libraries used as independent witnesses.

It must not depend on mechanism crates merely to bypass the facade.

### 6. Does a platform boundary participate in the contract?

Use shared parity fixtures plus native Swift/Kotlin tests.

A Rust test cannot prove:

- native lifecycle integration;
- language-specific ownership or cancellation;
- secure-provider boundaries;
- byte equivalence across generated bindings;
- platform reconstruction semantics.

### 7. Does the claim depend on public infrastructure or credentials?

Use an opt-in live probe. Never make a public relay the sole proof of deterministic product semantics.

## Recommended test layers

| Layer | Owns | Typical tools |
|---|---|---|
| Unit/example | Local values and transitions | `#[test]`, table cases |
| Property/model | Broad invariants and sequences | `proptest`, reference models, state machines |
| Crate integration | One crate with realistic collaborators | `crate/tests/*.rs` |
| Facade integration | Combined product behavior | `crates/nmp/tests/*.rs` |
| Acceptance capstone | Selected human-readable public promises | Gherkin/Cucumber through `nmp` |
| Persistence/fault | Crash, reopen, ordering, ambiguity | deterministic fixtures and failure points |
| Parity/native | Cross-language/platform contract | `nmp-parity`, Swift, Kotlin |
| Live probe | Compatibility with real providers/networks | ignored/opt-in tests |

## Setup may be privileged; behavior may not

A deterministic test environment may need to provide:

- a temporary store;
- a fake clock;
- scripted relays;
- exact identities;
- controlled DNS/network policy;
- injected process death;
- a prebuilt engine using a sanctioned test constructor.

This is fixture control.

It becomes a boundary violation when the setup performs the behavior being tested or the assertions read private state instead of the claimed contract.

Allowed:

- seed a relay-list event at an indexer;
- start the facade with a temporary store;
- record contacts at a relay witness.

Not allowed for a discovery acceptance scenario:

- insert the resolved route directly into the resolver;
- assert only that the resolver now contains it;
- call an engine-private handle instead of the facade operation.

## Structural proof plus capstone

For important behavior, prefer this shape:

1. **Structural proof:** a mechanism-level property/model/integration test makes the bad state difficult or impossible across a broad space.
2. **Public capstone:** one facade-level example proves the consequence users and integrators care about.

Example:

- `nmp-router` property test proves only dependency-linked plans reroot.
- `nmp` facade integration scenario proves a literal subscription keeps returning Alice's content after the active account changes to Bob.

The capstone does not replace the property test. The property test does not prove the facade wiring.

## Avoid these placement mistakes

### Test in the edited crate

A change in `nmp-resolver` does not mean all new tests belong there. Ask who owns the guarantee.

### Feature file per crate

Crates are implementation boundaries. Feature files are behavioral memory. Mirroring the crate tree makes behavior drift during refactors.

### Cucumber for every invariant

Cucumber is useful for selected readable examples. It is poor at exhaustive algebraic, concurrency, and state-space proof.

### Only end-to-end proof

A full stack scenario may pass through one lucky schedule and provide poor diagnosis. Add the narrow structural proof.

### Only private-state proof

A private table assertion may prove implementation details while the public facade is broken. Add the public consequence when it matters.

### Duplicate evidence

Do not keep shell, Cucumber, and Rust versions of the same acceptance path indefinitely. Retain the clearest structural proof and the necessary capstone, then delete redundant harnesses after equivalence is demonstrated.

## Ownership examples

| Behavioral claim | Feature domain | Primary executable owner |
|---|---|---|
| Literal demand does not reroot with active account | `routing/` | `nmp-router`, plus `nmp` capstone |
| One relay's EOSE is not global completion | `evidence/` | acquisition/evidence owner, plus facade observation |
| Accepted write survives restart | `writes/` or `lifecycle/` | `nmp`/store persistence integration |
| Invalid signer response cannot promote a write | `writes/` if product-meaningful | `nmp-signer` state-machine tests |
| NIP-defined bytes match across SDKs | `protocol-composition/` | protocol crate + `nmp-parity` + native tests |
| No destination outside plan is contacted | `must-never/` | router property + independent relay witness |

## Before adding a new test target

Ask:

- What contract cannot be expressed in an existing owner?
- Will the new target act as a consumer or acquire privileged internals?
- Does it reduce maintained complexity, or create another harness that needs its own proofs?
- Can the same evidence live as a normal integration target under the owning crate?

A new crate is an architectural decision, not a convenient folder.
