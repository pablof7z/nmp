# Protocol authoring

Use this workflow only when the desired behavior is absent from the supported facade and genuinely belongs to a NIP-aware module. Ordinary application composition stays in the app.

## Establish whether this is current or target work

Inspect the existing protocol crate before creating another module. Extend the established owner when the requested schema/context belongs there; for example, `nmp-nip29` already owns current group demand/composition helpers even though it deliberately makes no broad claim over all NIP-29 kinds.

The repository's generic `nmp-ownership` claim vocabulary and design-level `ModuleRegistration` composition are provisional building blocks, not a wired consumer API. There is currently no engine registration door, construction-time collision gate, or workspace-wide ownership audit, so formal ownership enforcement is target work. Do not tell an app to register modules at runtime or pass registrations to today's `Engine` unless the current facade proves that wiring exists. A design may propose the governed surface, but must label it target work and list its prerequisites.

Check foreign-schema prerequisites too. Current NIP-23 support is decode-oriented: decoded `Article` includes existing event identity and is not an authorless write draft. A design that composes an article must first add or identify a foreign-owned immutable public draft/writer rather than pretending one ships.

Rust protocol crates may be opt-in dependencies, but current Swift/Kotlin packaging bundles the projected NIP-29 helpers into the base facade. Do not describe native protocol packaging as opt-in until separate products/artifacts and parity tests exist.

## Decide the owner

Write one sentence for each question before code:

1. Which exact NIP schemas, kinds, and tags does this module own?
2. Which participating content remains owned by another module or the app?
3. Does the operation contribute public context, private authority, or only parsing/composition?
4. Which invariant cannot be preserved by an ordinary public `Filter`, `Demand`, or `WriteIntent`?
5. Which platforms must expose the semantic operation now?

Reject broad kind ranges, content-category ownership, or “because this protocol can contain it” claims. A dependency does not transfer schema ownership.

## Choose a closed output

Prefer one of these outputs:

- a public filter/binding/demand graph;
- a typed value reconstructed from ordinary live demand;
- an immutable unsigned draft;
- opaque validated source/access/routing context; or
- a semantic operation that resolves into the core query/write nouns.

Do not add a callback registry that chooses routing, admission, signer, ordering, or filters later. If the grammar lacks a required concept, propose a closed public value with hashing, equality, persistence, diagnostics, and projection semantics.

## Read path workflow

1. Define exact protocol input values and validate them before demand opens.
2. Express selection and source/access authority as printable closed demand.
3. Reconstruct protocol state from canonical query rows; do not keep a second event store.
4. Return semantic values plus the ordinary source evidence required to interpret them.
5. Bind nested observations to a typed resource with deterministic close/drop.
6. Preserve host/private context so equal filters under different authority do not share proof incorrectly.

## Write path workflow

1. Accept semantic app input, not raw tags for fields the module owns.
2. Compose immutable stages; each module contributes only its own fields.
3. Fail before acceptance on validation or authority conflict.
4. Let core resolve/pin the signer, accept the canonical pending row, sign once, and route through the durable outbox.
5. Preserve core receipt facts. Map only module-owned composition failures.
6. Never mutate a signed event, access signer secrets, write store indexes, open transport, or create an optimistic row lane.

NIP-29 is the practical model: app code supplies group identity and message semantics; the module derives protocol tags/context and returns a composed intent whose pinned-host routing cannot be forged as a generic relay list.

## Rust, FFI, and native projection

A consumer-facing semantic change is not complete at the Rust implementation alone.

1. Add the narrow direct-Rust API in the facade or opt-in protocol crate.
2. Decide whether it changes supported surface under repository governance.
3. Project closed records/enums and semantic operations through FFI. Prefer one-step compose-and-publish when no intermediate value is useful. When staged composition is required, the current supported precedent is an opaque, take-once semantic token such as `GroupSendIntent` followed by `publishComposed`; do not expose raw authority, routing, callbacks, or mutable mechanism internals.
4. Wrap generated types in idiomatic Swift and Kotlin ownership shapes.
5. Preserve semantic parity while allowing native lifecycle syntax to differ.
6. Update supported-surface docs, known gaps, and the surface change log when required.
7. Rebuild generated bindings and compile consumers from a clean-clone shape.

Choose and document one native shape. Do not leave callers guessing between one-step semantic publish and a staged take-once token. If the operation derives current time, add an internal fixed-clock/test seam so Rust/FFI/Swift/Kotlin can prove byte parity without exposing app-selected timestamps.

If one native platform is deliberately deferred, document the gap precisely. Never imply parity from generated bindings compiling.

## Required falsifiers

Prove the following where applicable:

- unowned schemas cannot be claimed;
- reusable binding/demand expansion matches its raw equivalent;
- state reconstruction uses the canonical store/query path;
- app relay arrays cannot forge module-owned private or pinned authority;
- cross-module composition is deterministic and preserves foreign fields;
- core signs the final body once;
- disabled module builds leave raw NMP useful;
- no module-owned store, signer, lifecycle, retry, or transport path appears;
- Rust, Swift, and Kotlin agree on final bytes and observable facts; and
- time-derived composition uses a fixed internal test clock/fixture rather than caller-selected production time; and
- teardown and resource-pressure refusal remain bounded.

## Review stop signs

Stop and redesign if a proposal includes:

- a generic `relays` escape hatch for protocol authority;
- protocol-owned UI ranking, display names, date formatting, or navigation;
- a native-only raw-tag composer that bypasses the Rust semantic operation;
- app callbacks invoked from routing or store admission;
- early signing followed by tag mutation;
- a module registry that performs startup work;
- hidden polling/retry beside engine ownership; or
- compatibility aliases retaining an unsafe path after the governed replacement lands.

Use the [protocol-module plan asset](../assets/protocol-module-plan.md) before implementation and the [feature review asset](../assets/feature-review.md) for the completed surface review.
