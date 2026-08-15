# Protocol authoring

Use this workflow only when the desired behavior is absent from the supported facade and genuinely belongs to a NIP-aware module. Ordinary application composition stays in the app.

## Establish whether this is current or target work

Inspect the existing protocol crate before creating another module. Extend the established owner when the requested schema/context belongs there; for example, `nmp-nip29` owns NIP-29's vocabulary — the `h` context row, the relationships between the three relay-signed kinds, and the composers for the moderation kinds 9000-9022 — while the app-facing door that retains a relay scope and mints the write lives in the `nmp` facade, because a lower crate cannot mint a `WriteIntent` without importing the write plane. `nmp-nip29` is engine-free by construction (`nostr` + `nmp-grammar`): it returns validated semantic values and never imports, constructs, stores or returns a `WriteIntent`.

Dependency direction decides where a door can live at all. `nmp-nip02` sits ABOVE the facade and depends on `nmp` — so it cannot be a facade feature; an `nmp -> nmp-nip02` edge is a package cycle cargo refuses to resolve. `nmp-ffi` binds it directly instead. Check the crate's actual dependency graph before proposing a facade re-export.

There is no ownership registry, and none is planned. The generic claim vocabulary and design-level module-registration composition the repository once carried were deleted in #859 (`nmp-ownership`, `nmp-audit`, and every protocol crate's `claims()` export), and #757/#758 — which would have wired them into routing — are closed NOT_PLANNED. A protocol crate owns a schema because it is the crate that defines, builds, and parses it, and because other crates consume its typed output rather than re-parsing the wire. Do not tell an author to declare claims, enroll in an audit, register modules at runtime, or pass registrations to `Engine`; none of those doors exist. A design proposing new governance must say what workspace-graph or API-boundary fact would enforce it, not propose a registry a future crate could decline to join.

Check schema prerequisites too. There is no NIP-23 owner at all: no crate, no `Article` type, no decode, and `NMPUI` does not decode it either. A design that composes an article must add that owner from scratch, decode included, rather than pretending one ships.

Rust protocol crates are opt-in Cargo features, and application-facing native packaging is compile-time selected too. One committed `.nmp.toml` names capabilities; `nmp prepare` materializes only the matching Swift/Kotlin sources and FFI feature set, so an unselected protocol name does not exist in that product. The repository's `Packages/NMP` and `Packages/NMPKotlin` trees are complete-surface qualification projects, not the package shape an application consumes. `NMPUI` and the desktop-JVM `:ui` child are qualification-only and are not in the prepared-product catalog.

Protocol values take decoded types, never bech32. `nip29::on` takes `RelayUrl`s, `Group::publish` takes a `PublicKey`, `set_following` takes a `PublicKey`, and `ListedSubject.pubkey` is a `PublicKey`. `npub`/`nevent`/`naddr` belong at the app's own user boundary; a proposed module signature carrying one is wrong regardless of how convenient the caller finds it.

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
4. Let core resolve/pin the signer, accept the canonical pending row, sign once, and route through publish queue.
5. Preserve core receipt facts. Map only module-owned composition failures.
6. Never mutate a signed event, access signer secrets, write store indexes, open transport, or create an optimistic row lane.

Two shapes exist, and the choice is about what the door must retain.

When the steps resolve entirely to an ordinary public `WriteIntent` that needs
nothing retained, return that noun directly. Do not add a take-once wrapper or a
second publication overload for symmetry. NIP-22 is the precedent: its top-level
composer returns `WriteIntent`, then the app uses generic `publish`.

When the door must retain state that the intent cannot carry, own the publish
step instead and return the ordinary `ReceiptStream`. NIP-29 is that precedent:
`nip29::on(hosts)` retains a nonempty host *set*, `group(id)` narrows it to one
group and `groups(ids)` to the several a single event belongs to, and
`Group::publish(&engine, author, builder)` appends the `h` row(s) BEFORE the
stamp/sign step — so context is inside the signed bytes — routes `Explicit` to
every host in the scope, and hands back the same receipt stream every other
write returns. Its intent mint is private on purpose, and the reason is
concrete: a `WriteIntent` carries no derives at all — not `Clone`, not `Debug`,
not `Serialize` — so an app holding one could not persist it across a restart,
batch it, or inspect it. Handing one out would buy nothing and would create a
second write lifecycle to keep honest.

Note the arity lesson from `groups(ids)`: widening a retained context is a
larger arity of the SAME door, not a second mechanism. `Group`'s write half is
literally a one-element `Groups`. If a proposal answers "several" with a
parallel type, a flag, or a per-call override, that is the shape to reject.

C7 independently owns kind:9 and its `e` replies. Notification `p` policy
remains app-owned. Mentions do NOT: the one tagging door's `with_content`
emits a mention's rendered token and its `p` row from the same call, precisely
so an app cannot write the two apart. A new module composing inline references
uses that door rather than emitting rows beside hand-written content.

## Rust, FFI, and native projection

A consumer-facing semantic change is not complete at the Rust implementation alone.

1. Add the narrow direct-Rust API in the facade or opt-in protocol crate.
2. Decide whether it changes the supported API.
3. Project closed records/enums and semantic operations through FFI. Protocol-owned composition either returns the ordinary public `WriteIntent` for generic `publish` (NIP-22), or owns the publish step and returns the ordinary `ReceiptStream` (NIP-29). Preserve immutable staged composition either way. If contextual authority is not safely representable, fix the one write noun so the authority is non-forgeable and payload-bound; do not create an opaque parallel intent, take-once lifecycle, raw routing escape hatch, callback, or mutable mechanism object.
4. Wrap generated types in idiomatic Swift and Kotlin ownership shapes.
5. Preserve semantic parity while allowing native lifecycle syntax to differ.
6. Update known gaps and affected docs when required.
7. Rebuild generated bindings and compile consumers from a clean-clone shape.

Pick ONE of the two shapes above for a given operation and document it; do not
project both for symmetry. If the operation derives current time, add an
internal fixed-clock/test seam so Rust/FFI/Swift/Kotlin can prove byte parity
without exposing app-selected timestamps. Prefer naming no clock at all:
composition that leaves `created_at` unset lets the engine stamp at acceptance,
which is what every projected composer already does.

Generic bounds do not cross UniFFI. Where direct Rust takes `&impl RootScope`,
FFI and the wrappers take a closed enum over the shapes an app actually holds —
`CommentTarget` is `.root(CommentRoot) | .row(Row)`. Design the closed enum
from what a caller holds, not by mechanically enumerating the trait's
implementors.

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
- hidden polling/retry beside engine ownership;
- a bech32 string in any module-owned parameter, field, or FFI argument; or
- compatibility aliases retaining an unsafe path after the replacement lands.

Use the [protocol-module plan asset](../assets/protocol-module-plan.md) before implementation and the [feature review asset](../assets/feature-review.md) for the completed review.
