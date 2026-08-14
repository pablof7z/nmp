---
name: nmp
description: Build, review, debug, test, or plan applications and protocol modules that use the Nostr Multi-Platform (NMP) Rust, Swift, or Kotlin public facade. Use for live queries, write intents and receipts, identity/signers, diagnostics, content parsing and NMPUI, the projected protocol doors (NIP-02, NIP-18, NIP-22, NIP-25, NIP-29, NIP-51, NIP-65, Blossom, NIP-C7), lifecycle and recovery, practical feature recipes, protocol extension, and consumer-facing API verification. Do not use this as authority for unverified internals or future VISION contracts.
---

# NMP application development

Use NMP as an embeddable engine with two app-facing nouns: a live query and a write intent observed through a receipt. Keep navigation, ordering, moderation, presentation, account UX, and product policy in the app.

## Establish current truth first

Verified-Revision: `4f9fbd544fe337fb0bfcc13de4cfb7ca6281a5c7`

This is the audited revision of the declared product/source authorities, not the skill package's own commit. A newer checkout is not automatically stale when only skill files changed; the bundled validator proves whether any declared source drifted.

1. Find the NMP repo root and read `README.md`, `docs/known-gaps.md`, and `docs/VISION.md`.
2. Record `git rev-parse HEAD`. If the checkout differs from the verified revision, inspect the current facade files listed in [Source map](references/source-map.md) before naming APIs.
3. Identify the consumer tier: direct Rust (`nmp`), Swift (`NMP`), Kotlin/JVM (`com.nmp.sdk`), or optional content/UI packages. Never substitute an internal crate or raw generated UniFFI type for its supported wrapper.
4. Check `docs/known-gaps.md`. Treat `docs/VISION.md` as the north star, not proof that a public method exists.

If asked to modify the NMP repository, follow its `AGENTS.md`: capture an issue first, use an isolated worktree and PR, update every affected projection, and test the touched API.

## Route the task

- Architecture, ownership, lifecycle, or implementation plans: [Application workflow](references/application-workflow.md)
- Concrete feed, profile, group, follow, publishing, offline, and debugging shapes: [Practical recipes](references/practical-recipes.md)
- Filters, bindings, demand, rows, evidence, or pagination: [Queries](references/queries.md)
- Publish, receipts, durability, accounts, local or remote signers: [Writes and identity](references/writes-and-identity.md)
- Restart, sign-out, reset, reconnect, teardown, or resource pressure: [Lifecycle and recovery](references/lifecycle-and-recovery.md)
- Relay proof screens, acquisition state, or debugging: [Diagnostics](references/diagnostics.md)
- Rust/Swift/Kotlin setup, call maps, and test commands: [Platforms](references/platforms.md)
- Content parsing, NMPUI, or any projected protocol door (NIP-02, NIP-18, NIP-22, NIP-25, NIP-29, NIP-51, NIP-65, Blossom, NIP-C7): [Content and protocols](references/content-and-protocols.md)
- Adding or reviewing a protocol module or cross-platform API: [Protocol authoring](references/protocol-authoring.md)
- Test strategy, falsifiers, restart proof, or live smoke verification: [Verification](references/verification.md)
- Exact implementation authority: [Source map](references/source-map.md)
- Maintaining or forward-testing this skill: [Evaluation protocol](references/evaluation.md) and [raw prompts](references/evaluation-prompts.md)

For a requested deliverable, copy and fill the appropriate reusable asset instead of inventing another format: [application plan](assets/application-plan.md), [protocol-module plan](assets/protocol-module-plan.md), [feature review](assets/feature-review.md), or [verification record](assets/verification-record.md).

## Non-negotiable guardrails

- Do not claim global `synced`, completeness, or authoritative emptiness. Report rows, per-source evidence, and explicit shortfalls.
- Do not build a second authoritative event cache or optimistic pending-row mirror in app state. Accumulate the delivered row stream for presentation state.
- Keep query ownership explicit. Swift observation is eager and cancelable; Kotlin `Flow` is cold and each collection subscribes unless the app shares it.
- A publish call is not convergence. Retain and observe the receipt; persist its id when restart reattachment matters.
- Do not expose private keys or opaque session payload bytes in logs, fixtures, screenshots, or source. NMP ships no credential store: the app must store the one whole-session payload as a sensitive atomic value.
- Do not promise app-controlled retries, typed pending-row metadata, or standard platform-vault account stores: those are not current public capabilities. `RelayWaiting::BackingOff` is evidence from NMP's engine-owned durable scheduler, not an app retry door. Write cancellation before signing, publish-queue enumeration and entry removal, correlation-token reattachment, `maxRelays` config, and populated `AuthPhase` do exist on the native tiers — check the current surface before calling something absent. No tier exposes any application-configurable worker/task/thread capacity: #704 removed it entirely.
- Treat governed sign-only as a cancellable operation, not a write. It freezes the active author, validates the exact signed result, and creates no pending row, receipt, route, relay attempt, storage fact, or publication claim. A pending external Rust signer resolves through NMP's opaque `PendingSignerSender`; do not expose or depend on its internal channel.
- There is no application-visible worker/task/thread ceiling: logical waits run as async tasks on one shared engine-owned runtime, while private physical bounds use cancellable backpressure. `EngineStartFailed { component, reason }` is raised only when the engine cannot be constructed, and `ObservationUnavailable { reason }` only when an ordinary or windowed `observe` cannot open its initial canonical projection after store degradation. They are not the whole of `EngineError`: store-open, relay-URL, window-validation, `LiveQuery` construction, `AuthCapabilityRegistryFull { limit }`, and `EngineClosed` are separate typed doors, and the capability-registry bound is a genuine app-configured ceiling. Relay connection/worker failure remains acquisition evidence and never constructs `ObservationUnavailable`. Direct Rust uses the owning `EngineError` variant, raw UniFFI uses `FfiError`, and Swift/Kotlin map them to the corresponding `NMPError`. Never relabel either outcome as a timeout or panic.
- Treat NIP-11 as an explicit engine-owned one-shot, not a relay stream or an app-owned cache. One engine admits at most 8 distinct HTTP/DNS/body flights; excess callers suspend cancellably in their own futures and same-relay callers share one completion, with no public capacity refusal. Service closure, credentialed-URL refusal, HTTP failure, size refusal, and invalid JSON are distinct acquisition facts. A stale-on-error snapshot preserves the last-good document and carries the refresh error separately. Reducer advertisement evidence exists only for relays in the current read plan; diagnostic freshness is derived from the engine clock and the cited document deadline. Relay advertisement never becomes behavioral capability proof.

## Completion gate

Before presenting code or a plan:

1. Verify every named type, method, throwing boundary, and error case in the current supported facade.
2. State platform-specific gaps that affect the design.
3. Show deterministic query/signer/engine teardown and explicit fact-stream ownership. Detaching a receipt fact stream (`ReceiptStatus.cancel()`, or ending the Kotlin collection scope) is distinct from `NMPEngine.cancel(receiptId:)`, which ends a not-yet-signed write obligation.
4. Include the exact build/test commands for the chosen tier.
5. Separate what the app owns from what NMP owns.
6. For runnable work, test the running consumer path; compilation alone is not proof of relay, signer, receipt, or lifecycle behavior.
