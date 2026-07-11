# Routing/retraction shared-file collision map

This is the extracted sequencing appendix for
`routing-build-plan.md`. `retraction-and-negative-deltas.md` and the routing
family touch the same engine files; coordinate their landing order.

| File | Retraction touches | Routing touches | Collision |
|---|---|---|---|
| `crates/nmp-engine/src/core/mod.rs` `on_relay_frame` event arm | store-commit removed-event handling | indexer write-back and provenance reads | **HIGH** |
| `core/mod.rs` `EngineCore` and constructor | displaced/deadline state | claim, directory, and policy state | **HIGH** |
| `core/mod.rs` receipt terminals / pending write | compensation and retraction | route status/resolution | **MEDIUM** |
| `outbox/mod.rs` write status | consumes terminal states | owns routed-shape change | **LOW** |
| `crates/nmp-store` insert door | supersession/removal/expiry | provenance/coverage reads | **LOW** |
| `crates/nmp-engine/src/runtime/mod.rs` engine loop | deadline receive | none | **NONE** |

Recommended order:

1. Land routing units A/B and retraction store-door symmetry in parallel; they
   are predominantly separate crates.
2. Serialize `core/mod.rs` work. Let one family land its constructor and event
   arm restructure, then rebase the other, or coordinate those touches in one
   worktree.
3. Do not run routing Unit H and the retraction `on_relay_frame` edit in
   independent worktrees simultaneously.

The constructor signature and event arm are the highest-conflict points.
