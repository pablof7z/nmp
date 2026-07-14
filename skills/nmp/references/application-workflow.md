# Application workflow

## Define ownership before code

NMP owns canonical event storage, relay planning and transport, live-query invalidation, signer orchestration, durable write obligations, per-relay outcomes, and proof surfaces. The app owns screens, ordering, moderation, formatting, navigation, account UX, feature policy, and when observations exist.

Avoid these boundary errors:

- app-maintained websocket subscriptions or relay routing beside NMP;
- a second durable event cache treated as truth;
- a boolean `isSynced` derived from EOSE or one relay;
- a view creating an unbounded observation every render;
- treating publish acceptance as delivered-to-all;
- collapsing typed thread-unavailability into a timeout, crash, or empty stream;
- importing internal crates or generated bindings to bypass an ergonomic gap.

## Build a vertical slice

1. Choose one user-visible query and define its matching `Filter`.
2. Choose source authority deliberately. A bare filter defaults by shape: author-bound filters use author outboxes; authorless filters use public/operator lanes. Use explicit `Demand` for pinned authority or strict pinned-cache provenance.
3. Start one observation at the feature/lifecycle owner. Rust consumers accumulate deltas by id; Swift/Kotlin replace from each already-accumulated `RowBatch` snapshot. Render app-owned order.
4. Render acquisition evidence and shortfalls as facts, not a global verdict.
5. If the feature writes, construct one `WriteIntent`, retain the receipt, and model per-relay outcomes.
6. Bind query, content-session, signer-session, engine teardown, and receipt-observation tasks to deterministic owners. Preserve engine/query errors, terminal action statuses, direct signer-connection errors, and native streamed signer-session failures as distinct facts at their owning boundaries. Cancelling receipt observation does not cancel a durable write obligation.
7. Add a bounded running proof using a real or scripted relay. Include restart proof for durable receipts or persistent cache claims.

## Review checklist

- Does every API name exist for the selected platform?
- Does any target/internal behavior masquerade as current public behavior?
- Is relay authority explicit where the default is unsuitable?
- Are rows accumulated from the SDK stream instead of mirrored from write intent?
- Can every observation and connection be cancelled promptly?
- Does the receipt UI retain ambiguity (`OutcomeUnknown`) and per-relay rejection?
- Are identity persistence and destructive store reset separate operations?
- Are secrets absent from logs and source?

For repo implementation work, surface changes are governed. Inspect `docs/surface/`, `docs/surface-change-log.md`, and the base-loaded surface CI before changing Rust, FFI, or native wrappers.
