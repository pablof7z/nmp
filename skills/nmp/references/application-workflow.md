# Application workflow

## Define ownership before code

NMP owns canonical event storage, relay planning and transport, live-query invalidation, signer orchestration, durable write obligations, per-relay outcomes, and proof surfaces. The app owns screens, ordering, moderation, formatting, navigation, account UX, feature policy, and when observations exist.

Avoid these boundary errors:

- app-maintained websocket subscriptions or relay routing beside NMP;
- a second durable event cache treated as truth;
- a boolean `isSynced` derived from EOSE or one relay;
- a view creating an unbounded observation every render;
- treating publish acceptance as delivered-to-all;
- collapsing a typed `EngineStartFailed` or `ObservationUnavailable` infra outcome into a timeout, crash, or empty stream;
- importing internal crates or generated bindings to bypass an ergonomic gap.

## Build a vertical slice

1. Choose one user-visible query and define its matching `LiveQuery`. One filter or demand is the one-branch case; use several branches when the feature genuinely unions distinct demands, and read `Frame.evidence` by branch index.
2. Say nothing about routing unless you mean to override NMP. A `Demand` that names only its selection gets `ReadRouting::Auto`: NIP-65 outbound relays for every author the selection resolves, relay hints and prior provenance, then the operator's app and fallback lanes. `Auto` is total — an authorless selection is the same path with no authors to solve for, not a different one — so there is nothing to choose and no routing error to handle. Name `ReadRouting::Explicit(relays)` only when the app genuinely means "ask these and nothing else"; it is never widened, and an empty set is refused at construction. A non-default `access` (NIP-42) and strict cache provenance over an `Explicit` set are separate axes, set independently.
3. Start one observation at the feature/lifecycle owner. Consumers accumulate deltas by id from each `Frame`. Render app-owned order.
4. Render acquisition evidence and shortfalls as facts, not a global verdict.
5. If the feature writes, construct one `WriteIntent`, retain the receipt, and model per-relay outcomes.
6. Bind query, signer-session, engine teardown, and fact-stream consumption to deterministic owners. There is no worker/task admission ceiling: observers, actions, and signers run as async tasks on one shared engine-owned runtime, so ordinary concurrent operations just make progress. Preserve the genuine infra outcomes (`EngineStartFailed` at construction, `ObservationUnavailable` for a live observe) and terminal action statuses as distinct facts at their owning boundaries. Detaching a receipt fact stream is distinct from cancelling the write: dropping the `ReceiptStream`'s `statuses` receiver stops delivery, while `Engine::cancel(id)` ends a not-yet-signed obligation.
7. Add a bounded running proof using a real or scripted relay. Include restart proof for durable receipts or persistent cache claims.

## Review checklist

- Does every API name exist on the `nmp` facade and the protocol crates the app actually depends on?
- Does any target/internal behavior masquerade as current public behavior?
- Is relay authority explicit where the default is unsuitable?
- Are rows accumulated from the frame stream instead of mirrored from write intent?
- Can every observation and connection be cancelled promptly?
- Does the delivery UI keep the `WriteFact::Destinations { complete, .. }` flag separate from delivery, and preserve per-relay `RelayState::Rejected`/`AuthFailed`/`GaveUp` rather than collapsing them? "Still determining where to send" and "nowhere to send" are the two sides of that one branch, not one sentence.
- Are identity persistence and destructive store reset separate operations?
- Are secrets absent from logs and source?
