# Supported product surface

The `nmp` crate is the canonical Rust product facade. Applications construct an
`nmp::Engine` and use the two workload nouns—live queries and write intents—plus
identity and diagnostics. Mechanism crates such as `nmp-engine`, `nmp-store`,
`nmp-router`, `nmp-resolver`, and `nmp-transport` are implementation/test seams,
not parallel application contracts. The feature-gated `from_parts` path is
explicitly unstable test infrastructure.

Opt-in direct-Rust protocol crates may provide semantic operations over that
same facade. `nmp-nip65` exposes the first kind:10002 bootstrap operation: its
consumer manifest needs only `nmp` plus `nmp-nip65`, and the operation returns
the ordinary `ReceiptStream`. The internal exact-route authority remains
withheld from `nmp`; no raw relay-array write escape hatch is added.

NIP-65 bootstrap is not projected through UniFFI, Swift, or Kotlin in its first
unit. Native consumers must not recreate it with raw sockets or mechanism
crates; parity is an explicit later surface change.

NIP-22 comment composition is projected, but it does not become an Engine
capability. `nmp-nip22` owns the kind:1111/NIP-73 schema and returns the
ordinary `WriteIntent`; FFI, Swift, and Kotlin expose matching engine-free
`comment_intent`/`commentIntent` free functions returning
`FfiWriteIntent`/`WriteIntent`. Publication uses the existing generic
`publish` door and receipt lifecycle. There is no `Engine.commentIntent`,
`CommentIntent` wrapper, or NIP-22-specific composed-publication overload.

NIP-29 Group publication is currently a direct-Rust surface.
`nmp_nip29::Group` mints the host-pinned read demand and the complete
explicit-host `WriteIntent`; `nmp::GroupOperations` submits that value through
the ordinary `Engine::publish` lifecycle. No FFI or Swift Group publication
door exists yet—only read-only `groupDiscoveryDemand`. Issue
[#1015](https://github.com/pablof7z/nmp/issues/1015) owns that projection, and
native apps must not reproduce `h`, host routing, signing, or receipt
choreography while it is absent. This record makes no Kotlin or Android Group
support claim.

`nmp-ffi` is a projection of that facade. The repository uses UniFFI proc
macros and extracts component metadata from a compiled library; there is no UDL
source of truth. Swift and Kotlin add native observation/lifecycle ergonomics
over generated bindings without becoming independent semantic engines.

## How public changes are governed

- `docs/surface/nmp-facade.txt` starts with pinned `cargo-public-api` output for
  facade-owned items, then adds compiler-resolved definitions for every
  dependency-owned item explicitly re-exported from the `nmp` root, plus the
  dependency-owned definition closure reachable through its public shape.
  Discovery comes from rustdoc JSON `use` items and follows only exact Cargo
  package/library identities; unrelated engine APIs are never traversed.
- `docs/surface/components/<key>/component.toml` is the closed-schema catalog
  record for one UniFFI namespace. Every active record has a sibling
  `uniffi.txt`, a deterministic language-independent rendering of that exact
  namespace's proc-macro metadata extracted in library mode. The former
  aggregate `docs/surface/nmp-ffi-component.txt` path is forbidden.
- A self-owning record declares its Cargo package/manifest/library stem and is
  built once in a target directory isolated to that artifact owner. A
  co-located interface names that owner and declares no library fields;
  regeneration extracts each namespace from the one owner build. The catalog
  checks the declared package and library stem against the exact Cargo
  manifest. Active package/build ownership may move through a governed
  transition; the UniFFI namespace and declared Swift/Kotlin manifest/source
  roots remain stable.
  Swift and Kotlin each declare both manifest and source roots or an explicit
  omission reason. The optional Android table is all-or-nothing and records
  Gradle project, namespace, Maven coordinate, manifests, and sources.
- Active identities and retired reservations are globally unique. Retirement
  is the checker's exact derived tombstone: the snapshot and every derived
  owned path disappear, package/library/namespace/Android identities stay
  reserved, tombstone bytes are immutable, and an owner cannot retire while a
  live child names it. The catalog is capped at 128 records, descriptors at
  32 KiB, and each UTF-8, LF-only, NUL-free component snapshot at 20,000 lines
  / 2,000,000 bytes.
- The extractor is a standalone locked program under
  `tools/component-interface-snapshot`, outside the NMP workspace and product
  crate. Steady-state regeneration copies its manifest, lockfile, and source
  from the PR base before inspecting the head-built native library. It refuses
  an undeclared namespace anywhere in that library and requires the requested
  catalog namespace exactly once; declared dependency namespaces may therefore
  coexist without becoming an ungoverned extra.
- The Rust re-export resolver is likewise a standalone locked program under
  `tools/rust-facade-snapshot`. It checks rustdoc JSON format 60, resolves
  renamed extern aliases through Cargo's exact `PackageId` and library target,
  and recursively serializes every dependency-owned definition reachable from
  the explicit re-export (including nested structs, enums, aliases, signatures,
  generics, and where clauses). Cycles and repeated definitions become stable
  semantic references; docs, spans, numeric ids, private/unreachable items, and
  trait/auto/blanket impl inventories never enter the projection. Public
  inherent methods and associated constants/types are retained only for the
  definitions explicitly re-exported at the `nmp` root—not for recursively
  reached helper/data types. Their signatures, generics, bounds, deprecations,
  and stable user-authored attributes are part of the usable facade shape;
  compiler cfg/inline traces and source paths are discarded. Mixed tuple fields
  preserve public field definitions and stable
  `null` privacy markers without exposing rustdoc ids. A path-free reference
  must have an indexed semantic definition or extraction fails rather than
  emitting an unstable rustdoc id. Metadata and rustdoc run locked, so manifest
  and lockfile drift fails closed. Regeneration also enforces a 30,000-line /
  8,000,000-byte ceiling: deliberately generous for the explicit facade roots,
  but small enough to catch accidental recursive helper-method expansion.
- `scripts/regenerate-surface-snapshots.sh` regenerates the facade and every
  active component from a clean checkout. The ordinary checker runs it in both
  catalog and reverse component order, requires byte-identical output, then
  compares every generated file with the committed head.
- Cross-SDK exceptions use one closed TOML record per exact
  `(component, concept, platform)` tuple in canonical tuple order. The parity
  report keeps both active suppressions and currently unused exceptions
  visible.
- Any baseline change, any change below the public Swift/Kotlin wrapper paths,
  or a change to their consumer package/build/settings manifests must append a
  schema-complete entry to
  `docs/surface-change-log.md`. Correction-only appends are allowed; previously
  merged bytes cannot be edited, deleted, rewritten, or reordered.
- Evidence-path recognition belongs to `scripts/check-surface-governance.sh`
  through its configured `SURFACE_CHANGE_LOG`; the component catalog tool
  classifies public projections only and does not hard-code an evidence path.
- The pull-request checklist makes failure evidence and cross-surface,
  persistence, diagnostics, falsifier, removal, and signoff consequences
  explicit.

## Trusted execution and the bootstrap boundary

The steady-state gate uses `pull_request_target`, so GitHub loads the workflow
from the default branch. It extracts the checker, regenerator, installer,
falsifiers, and tool pins from the PR's base commit, then judges the proposed
head with those trusted files. The head is checked out without persisted
credentials under a read-only token and no secrets. That trusted-target job
treats the head strictly as git data: it never compiles head code and never
executes a head file. The base checker rejects changes to its own workflow,
ordinary regeneration workflow, scripts, and tool pins, preventing a PR from
replacing its judge, companion regeneration check, catalog, or component
extractor with an `exit 0`/stale-output program.

Deterministic cargo-public-api/component regeneration runs separately in the
ordinary `pull_request` trust domain, where compiling proposed code belongs.
That job has no secrets or persisted credentials, and the trusted-target gate
proves its workflow and invoked scripts are byte-identical to the base. Issue
#81 must require both stable checks: `surface-governance` and
`surface-regeneration`.

The component-catalog introduction is an explicit two-phase bootstrap. While
the base has the legacy aggregate snapshot and no catalog tool, credential-free
ordinary CI runs the complete proposed catalog/checker/regenerator and requires
the exact two-record `nmp-core` + `nmp-nip46` transition. The existing
base-trusted target checker necessarily rejects replacement of its own program,
so only the repository owner's protected update procedure can land that exact
reviewed bootstrap. Once the catalog tool is on the default branch, both
workflows always extract the program from the base; the bootstrap flag is
removed by the ordinary regeneration wrapper before every steady-state
transition, and the checker independently refuses any signal that survives
outside the exact bootstrap.

The bootstrap checkpoint intentionally contains no fabricated change-log
entry. The real PR number/URL, independent reviewer, and signoff can only be
appended after those facts exist. Future component records for #952 and #824
are ordinary later catalog obligations, not prerequisites for the current
two-record bootstrap.

The checker receives the actual PR number/URL and independently derived changed
projection set from the trusted workflow. Entries must link that exact PR,
carry non-empty fields, and name exactly the affected projections. Snapshot and
history checks have no network dependency; only installing the pinned tool uses
the registry, with Cargo's locked install plus an explicit archive checksum.
