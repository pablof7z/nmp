# Source map

These are the authorities to inspect when a checkout differs from the verified revision. Paths are relative to the NMP repository root. Each `Source:` entry is checked by `scripts/validate_skill.py` when the repo is available.

## Product truth

- Source: `AGENTS.md`
- Source: `README.md`
- Source: `docs/VISION.md`
- Source: `docs/known-gaps.md`
- Source: `docs/builder/28-patterns.md`
- Source: `docs/design/async-observation-handles.md`
- Source: `docs/design/durable-write-signing-and-retry.md`
- Source: `docs/builder/15-editing-replaceable.md`
- Source: `docs/builder/19-offline-sync.md`
- Source: `docs/builder/22-diagnostics.md`
- Source: `docs/builder/23-threading-lifecycle.md`
- Source: `docs/builder/25-testing.md`
- Source: `docs/builder/31-gallery.md`
- Source: `docs/builder/32-extending.md`
- Source: `docs/design/protocol-modules-and-composition.md`

## Direct Rust facade and value types

- Source: `crates/nmp/src/lib.rs`
- Source: `crates/nmp/src/engine.rs`
- Source: `crates/nmp/src/engine`
- Source: `crates/nmp/src/error.rs`
- Source: `crates/nmp/src/config.rs`
- Source: `crates/nmp/src/subscription.rs`
- Source: `crates/nmp/src/observation.rs`
- Source: `crates/nmp/src/auth.rs`
- Source: `crates/nmp-grammar/src/binding.rs`
- Source: `crates/nmp-grammar/src/descriptor.rs`
- Source: `crates/nmp-grammar/src/live_query.rs`
- Source: `crates/nmp-grammar/src/selector.rs`
- Source: `crates/nmp-grammar/src/tagging.rs`
- Source: `crates/nmp-grammar/src/write.rs`
- Source: `crates/nmp/src/diagnostics.rs`
- Source: `crates/nmp-store/src/lib.rs`
- Source: `crates/nmp-store/src/persistence_failure.rs`
- Source: `crates/nmp-transport/src/thread_census.rs`
- Source: `crates/nmp-nip02/Cargo.toml`
- Source: `crates/nmp-nip02/src/lib.rs`
- Source: `crates/nmp-nip22/src/lib.rs`
- Source: `crates/nmp-nip22/src/intent.rs`
- Source: `crates/nmp-nip22/src/root.rs`
- Source: `crates/nmp-nip29/Cargo.toml`
- Source: `crates/nmp-nip29/src/lib.rs`
- Source: `crates/nmp-nip29/src/context.rs`
- Source: `crates/nmp-nip29/src/group_list.rs`
- Source: `crates/nmp-nip29/src/operations.rs`
- Source: `crates/nmp-nip29/src/simple_groups.rs`
- Source: `crates/nmp-nip73/src/lib.rs`
- Source: `crates/nmp-signer/src/op.rs`
- Source: `crates/nmp-content/src/document.rs`
- Source: `crates/nmp-content/src/parse.rs`

Use public declarations and tests as current API truth. Use `docs/VISION.md` for intended invariants, `docs/builder/28-patterns.md` for what the design structurally excludes, and `docs/known-gaps.md` for what is not built. Comments inside mechanism code can explain design but do not create a consumer API.
