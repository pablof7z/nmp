---
name: nmp
description: Build, review, debug, test, or plan applications and protocol modules that use the Nostr Multi-Platform (NMP) Rust, Swift, or Kotlin public facade. Use for live queries, write intents and receipts, identity/signers, diagnostics, NMPContent/NMPUI, NIP-02/NIP-29/NIP-46 helpers, lifecycle and recovery, practical feature recipes, protocol extension, and consumer-facing API verification. Do not use this as authority for unverified internals or future VISION contracts.
---

# NMP application development

Use NMP as an embeddable engine with two app-facing nouns: a live query and a write intent observed through a receipt. Keep navigation, ordering, moderation, presentation, account UX, and product policy in the app.

## Establish current truth first

Verified-Revision: `e3751b028a30129c70ad0c96f259149585c2137b`

This is the audited revision of the declared product/source authorities, not the skill package's own commit. A newer checkout is not automatically stale when only skill files changed; the bundled validator proves whether any declared source drifted.

1. Find the NMP repo root and read `README.md`, `docs/known-gaps.md`, and `docs/architecture/supported-surface.md` when present.
2. Record `git rev-parse HEAD`. If the checkout differs from the verified revision, inspect the current facade files listed in [Source map](references/source-map.md) before naming APIs.
3. Identify the consumer tier: direct Rust (`nmp`), Swift (`NMP`), Kotlin/JVM (`com.nmp.sdk`), or optional content/UI packages. Never substitute an internal crate or raw generated UniFFI type for its supported wrapper.
4. Check [Current surface and gaps](references/current-surface.md). Treat `docs/VISION.md` as the north star, not proof that a public method exists.

If asked to modify the NMP repository, follow its `AGENTS.md`: capture an issue first, use an isolated worktree and PR, update every affected projection, and test the touched surface.

## Route the task

- Architecture, ownership, lifecycle, or implementation plans: [Application workflow](references/application-workflow.md)
- Concrete feed, profile, group, follow, publishing, offline, and debugging shapes: [Practical recipes](references/practical-recipes.md)
- Filters, bindings, demand, rows, evidence, or pagination: [Queries](references/queries.md)
- Publish, receipts, durability, accounts, local or remote signers: [Writes and identity](references/writes-and-identity.md)
- Restart, sign-out, reset, reconnect, teardown, or resource pressure: [Lifecycle and recovery](references/lifecycle-and-recovery.md)
- Relay proof screens, acquisition state, or debugging: [Diagnostics](references/diagnostics.md)
- Rust/Swift/Kotlin setup, call maps, and test commands: [Platforms](references/platforms.md)
- NMPContent, NMPUI, NIP-02, NIP-29, or NIP-46 helpers: [Content and protocols](references/content-and-protocols.md)
- Adding or reviewing a protocol module or governed cross-platform surface: [Protocol authoring](references/protocol-authoring.md)
- Test strategy, falsifiers, restart proof, or live smoke verification: [Verification](references/verification.md)
- Exact implementation authority: [Source map](references/source-map.md)
- Maintaining or forward-testing this skill: [Evaluation protocol](references/evaluation.md) and [raw prompts](references/evaluation-prompts.md)

For a requested deliverable, copy and fill the appropriate reusable asset instead of inventing another format: [application plan](assets/application-plan.md), [protocol-module plan](assets/protocol-module-plan.md), [feature review](assets/feature-review.md), or [verification record](assets/verification-record.md).

## Non-negotiable guardrails

- Do not claim global `synced`, completeness, or authoritative emptiness. Report rows, per-source evidence, and explicit shortfalls.
- Do not build a second authoritative event cache or optimistic pending-row mirror in app state. Accumulate the delivered row stream for presentation state.
- Keep query ownership explicit. Swift observation is eager and cancelable; Kotlin `Flow` is cold and each collection subscribes unless the app shares it.
- A publish call is not convergence. Retain and observe the receipt; persist its id when restart reattachment matters.
- Do not expose secret keys in logs, fixtures, screenshots, or source. The bundled file account stores are explicitly insecure development conveniences, not Keychain/Keystore providers.
- Do not promise write cancellation, app-controlled retries, typed pending-row metadata, populated AUTH phases, native `maxRelays`, or secure native signer persistence: those are not current cross-platform public capabilities. Swift/Kotlin do expose `maxNativeTasks`; do not confuse that native-task ceiling with the Rust/raw-FFI relay ceiling.
- Keep finite-capacity refusal distinct from OS-thread refusal. A full zero-queue native executor returns `ExecutorSaturated { component, capacity }`; an OS spawn failure remains `ThreadUnavailable { component, reason }`. Direct Rust uses the owning `EngineError`, `FollowActionFailure`, or `Nip46Error` variant. Raw UniFFI uses `FfiError`, and Swift/Kotlin map synchronous setup failures to the corresponding `NMPError`. Refusal occurs before underlying stream/operation ownership transfers, but the NIP-02 action API deliberately returns its `FollowAction` first and reports worker refusal as a terminal failure on that handle. Once a native NIP-46 handle exists, an inner session/relay-worker failure may instead arrive as streamed `failed(reason)`/`Failed` followed by closure. Never match one boundary's type at another, treat saturation as a queue, or relabel either refusal as a timeout or panic.
- Treat NIP-11 as an explicit engine-owned one-shot, not a relay stream or an app-owned cache. Its flights share the same zero-queue native executor, each relay has a finite waiter set, and executor saturation, waiter saturation, OS-thread refusal, service closure, credentialed-URL refusal, HTTP failure, size refusal, and invalid JSON are distinct acquisition facts. A stale-on-error snapshot preserves the last-good document and carries the refresh error separately. Reducer advertisement evidence exists only for relays in the current read plan; diagnostic freshness is derived from the engine clock and the cited document deadline. Relay advertisement never becomes behavioral capability proof.

## Completion gate

Before presenting code or a plan:

1. Verify every named type, method, throwing boundary, and error case in the current supported facade.
2. State platform-specific gaps that affect the design.
3. Show deterministic query/content/signer/engine teardown and explicit receipt-consumption ownership. Swift/Kotlin receipts have no observer-detach handle: cancelling the app task/collector ends consumption, not the native bridge or write obligation.
4. Include the exact build/test commands for the chosen tier.
5. Separate what the app owns from what NMP owns.
6. For runnable work, test the running consumer path; compilation alone is not proof of relay, signer, receipt, or lifecycle behavior.
