---
name: nmp
description: Build, review, debug, or plan applications that consume the Nostr Multi-Platform (NMP) Rust, Swift, or Kotlin public facade. Use for live queries, write intents and receipts, identity/signers, diagnostics, NMPContent/NMPUI, NIP-02/NIP-29/NIP-46 helpers, platform setup, lifecycle, and consumer-facing API verification. Do not use this as authority for NMP internals or future VISION contracts.
---

# NMP application development

Use NMP as an embeddable engine with two app-facing nouns: a live query and a write intent observed through a receipt. Keep navigation, ordering, moderation, presentation, account UX, and product policy in the app.

## Establish current truth first

Verified-Revision: `5a508eaf5ad9d75e08645b41975e3312cf96aad7`

1. Find the NMP repo root and read `README.md`, `docs/known-gaps.md`, and `docs/architecture/supported-surface.md` when present.
2. Record `git rev-parse HEAD`. If the checkout differs from the verified revision, inspect the current facade files listed in [Source map](references/source-map.md) before naming APIs.
3. Identify the consumer tier: direct Rust (`nmp`), Swift (`NMP`), Kotlin/JVM (`com.nmp.sdk`), or optional content/UI packages. Never substitute an internal crate or raw generated UniFFI type for its supported wrapper.
4. Check [Current surface and gaps](references/current-surface.md). Treat `docs/VISION.md` as the north star, not proof that a public method exists.

If asked to modify the NMP repository, follow its `AGENTS.md`: capture an issue first, use an isolated worktree and PR, update every affected projection, and test the touched surface.

## Route the task

- Architecture, ownership, lifecycle, or implementation plans: [Application workflow](references/application-workflow.md)
- Filters, bindings, demand, rows, evidence, or pagination: [Queries](references/queries.md)
- Publish, receipts, durability, accounts, local or remote signers: [Writes and identity](references/writes-and-identity.md)
- Relay proof screens, acquisition state, or debugging: [Diagnostics](references/diagnostics.md)
- Rust/Swift/Kotlin setup, call maps, and test commands: [Platforms](references/platforms.md)
- NMPContent, NMPUI, NIP-02, NIP-29, or NIP-46 helpers: [Content and protocols](references/content-and-protocols.md)
- Exact implementation authority: [Source map](references/source-map.md)
- Maintaining or forward-testing this skill: [Evaluation protocol](references/evaluation.md) and [raw prompts](references/evaluation-prompts.md)

## Non-negotiable guardrails

- Do not claim global `synced`, completeness, or authoritative emptiness. Report rows, per-source evidence, and explicit shortfalls.
- Do not build a second authoritative event cache or optimistic pending-row mirror in app state. Accumulate the delivered row stream for presentation state.
- Keep query ownership explicit. Swift observation is eager and cancelable; Kotlin `Flow` is cold and each collection subscribes unless the app shares it.
- A publish call is not convergence. Retain and observe the receipt; persist its id when restart reattachment matters.
- Do not expose secret keys in logs, fixtures, screenshots, or source. The bundled file account stores are explicitly insecure development conveniences, not Keychain/Keystore providers.
- Do not promise write cancellation, app-controlled retries, typed pending-row metadata, populated AUTH phases, native `maxRelays`, or secure native signer persistence: those are not current cross-platform public capabilities. Swift/Kotlin do expose `maxNativeTasks`; do not confuse that native-task ceiling with the Rust/raw-FFI relay ceiling.
- Keep finite-capacity refusal distinct from OS-thread refusal. A full zero-queue native executor returns `ExecutorSaturated { component, capacity }` before the associated stream or operation transfers ownership; an OS spawn failure remains `ThreadUnavailable { component, reason }`. Direct Rust uses the owning `EngineError`, `FollowActionFailure`, or `Nip46Error` variant. Raw UniFFI uses `FfiError`, and Swift/Kotlin map synchronous setup failures to the corresponding `NMPError`. Once a native NIP-46 handle exists, an inner session/relay-worker failure may instead arrive as streamed `failed(reason)`/`Failed` followed by closure. Never match one boundary's type at another, treat saturation as a queue, or relabel either refusal as a timeout or panic.

## Completion gate

Before presenting code or a plan:

1. Verify every named type, method, throwing boundary, and error case in the current supported facade.
2. State platform-specific gaps that affect the design.
3. Show deterministic query/content/signer/engine teardown and explicit receipt-observation ownership. A receipt has no cancel operation and cancelling observation does not cancel its obligation.
4. Include the exact build/test commands for the chosen tier.
5. Separate what the app owns from what NMP owns.
6. For runnable work, test the running consumer path; compilation alone is not proof of relay, signer, receipt, or lifecycle behavior.
