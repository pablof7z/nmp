# Evaluation

Use these prompts after any material skill revision. Give each to a fresh agent with the skill and a current NMP checkout. The evaluator should inspect the named source files, then score the response against the criteria.

## Swift feed with publishing

Prompt: "Plan a SwiftUI home feed over followed accounts with local publishing, offline restart, delivery status, and a relay-debug sheet using NMP. Name the APIs and lifecycle owners."

Pass if the plan uses Swift public wrappers, distinguishes first-contact-list creation, consumes full `RowBatch` snapshots, retains a receipt id, reattaches after restoring the signer, limits diagnostics to native fields, and owns query/receipt-observation/engine lifecycles deterministically without claiming that observation cancellation cancels the obligation. Fail for typed pending-row metadata, cancel-write/retry methods, global sync, or native `maxRelays`.

## Kotlin Android-shaped host

Prompt: "Design an Android app architecture using the current Kotlin NMP package for a pinned-relay group timeline, NIP-46 handoff, and parsed references."

Pass if it calls the module a desktop-JVM falsifier, leaves Intent/package-manager work to the host, uses `groupContentDemand(host, groupId)` to obtain the explicit `NMPDemand`, shares cold flows, closes content claims, their `NostrContentSession`, the exact `NMPNip46Connection`, and the engine, and does not claim Compose/AAR/NIP-55 or Kotlin following support.

## Rust durable write

Prompt: "Show a direct-Rust service design that publishes a durable replaceable edit, survives restart, and reports honest delivery evidence."

Pass if it uses `Engine`, the compare-and-swap payload, `publish_tracked`, promptly persisted receipt id, `reattach_receipt`, signer restoration, current `WriteStatus` variants, and shutdown/reset separation. It must disclose stream-local pre-acceptance ids, limited reattachment replay, the no-enumeration crash window, and channel closure not being delivery success. Fail for internal `Handle`, public cancellation, or collapsing outcomes to success/failure.

## Adversarial API review

Prompt: "A design says every NMP row exposes pending intent/signature state; diagnostics show write attempts and queue pressure; Swift config sets max relays; Kotlin offers follow/unfollow. Review it."

Pass only if all four claims are rejected with current source pointers and the response distinguishes north-star/internal behavior from public wrappers.

Record the tested commit, agent, prompt, unsupported API inventions, missed gaps, and result. A polished answer that did not verify the current facade is a failure.
