# Supported product surface

The `nmp` crate is the canonical Rust product facade. Applications construct an
`nmp::Engine` and use the two workload nouns—live queries and write intents—plus
identity and diagnostics. Mechanism crates such as `nmp-engine`, `nmp-store`,
`nmp-router`, `nmp-resolver`, and `nmp-transport` are implementation/test seams,
not parallel application contracts. The feature-gated `from_parts` path is
explicitly unstable test infrastructure.

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
- `docs/surface/nmp-ffi-component.txt` is a deterministic,
  language-independent rendering of UniFFI proc-macro component metadata
  extracted in library mode.
- The extractor is a standalone locked program under
  `tools/component-interface-snapshot`, outside the NMP workspace and product
  crate. Steady-state regeneration copies its manifest, lockfile, and source
  from the PR base before inspecting the head-built native library.
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
- `scripts/regenerate-surface-snapshots.sh` regenerates both from a clean
  checkout; `scripts/check-surface-governance.sh` verifies them.
- Any baseline change, any change below the public Swift/Kotlin wrapper paths,
  or a change to their consumer package/build/settings manifests must append a
  schema-complete entry to
  `docs/surface-change-log.md`. Correction-only appends are allowed; previously
  merged bytes cannot be edited, deleted, rewritten, or reordered.
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
replacing its judge, companion regeneration check, or component extractor with
an `exit 0`/stale-output program.

Deterministic cargo-public-api/component regeneration runs separately in the
ordinary `pull_request` trust domain, where compiling proposed code belongs.
That job has no secrets or persisted credentials, and the trusted-target gate
proves its workflow and invoked scripts are byte-identical to the base. Issue
#81 must require both stable checks: `surface-governance` and
`surface-regeneration`.

This change is the unavoidable one-time bootstrap: the workflow cannot run
from a base on which it does not yet exist. It therefore seeds only the
historical #67/#73/#77 trail and must merge under the repository's existing CI
plus manual review. After merge, #81 enables both checks as required
branch-protection statuses. Governance-program updates then require an
explicit owner-controlled bootstrap/update rather than an ordinary
self-approved PR.

Issue #89 is the first owner-controlled update to this governance program. The
old trusted checker is expected to reject its protected workflow/script/tool
changes. Because #81 has not enabled those checks administratively, #89 must
merge through existing non-governance CI plus exhaustive local and independent
review; the resulting default branch then protects the resolver. Its change-log
entry is appended only after the real PR number, URL, reviewer, and signoff
exist—never fabricated in a pre-PR implementation commit.

The checker receives the actual PR number/URL and independently derived changed
projection set from the trusted workflow. Entries must link that exact PR,
carry non-empty fields, and name exactly the affected projections. Snapshot and
history checks have no network dependency; only installing the pinned tool uses
the registry, with Cargo's locked install plus an explicit archive checksum.
