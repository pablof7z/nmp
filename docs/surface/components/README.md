# Governed UniFFI components

Each directory below this one is one versioned UniFFI namespace. An active
component has exactly `component.toml` plus `uniffi.txt`; a retired component
has only its immutable `component.toml` tombstone. There is no aggregate
component snapshot or compatibility read path.

`component.toml` schema 1 is closed and parsed by the base-trusted
`tools/surface-component-catalog` program. Unknown fields and schema versions
fail. Every record names the namespace and the artifact owner that physically
contains it:

- A self-owning record has `artifact_owner = key` and alone declares
  `ffi_package`, `ffi_manifest`, and `library_stem`.
- A co-located namespace points `artifact_owner` at an active self-owning
  record and declares none of those three library fields. Ownership is depth
  one. Regeneration builds each owner once, then extracts each namespace
  independently.
- Every active record declares `ffi_sources`. Swift and Kotlin each declare
  both manifest/source roots, or two empty arrays plus a non-empty omission
  reason. A generated-only internal namespace is not falsely presented as an
  ergonomic app surface.
- The optional `[android]` table is all-or-nothing: `gradle_project`,
  `namespace`, `maven_coordinate`, `manifests`, and `sources`. These stable
  package identities are catalog data; versions and per-target ABI identities
  remain release provenance.

Descriptors are limited to 32,768 bytes, snapshots to 20,000 lines and
2,000,000 bytes, and the catalog to 128 records. Paths are relative,
canonical, and backed by regular files or trees in the exact Git revision.
Namespaces, library/package identities, Android identities, and source roots
cannot collide.

Retirement is the one-way `active -> retired` transition. The trusted tool
renders the exact tombstone from the base record and actual PR identity:
reserved namespace/artifact identities, the PR number and URL, and a sorted
`removed_paths` set derived from every declared active manifest/source root.
The snapshot and all derived paths must disappear in that same head. Tombstone
bytes, mode, and path are then immutable; every later check refuses path
resurrection, identity reuse, reactivation, deletion, or an owner retirement
while a live co-located child remains.

The former `docs/surface/nmp-ffi-component.txt` path is deleted and permanently
refused. `nmp-core/uniffi.txt` is the sole core namespace snapshot.

Future shared-interface and independently published protocol records from
issues #952 and #824 use this ordinary data path after their artifacts exist;
they are not placeholders in the #954 bootstrap.
