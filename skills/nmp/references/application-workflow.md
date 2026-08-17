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

0. Declare the compile-time surface before writing a line of feature code. An app commits one `.nmp.toml` naming its capabilities and products, then runs `nmp prepare`. Following, comments, groups, reposts, reactions, chat replies, lists, `blossom` (`Blossom uploads`), verified assets, rich content, and outbox routing are accepted capability names — an unselected one is not a runtime gap, it is a name that does not exist at compile time. Add the capability rather than reaching around it.
1. Choose one user-visible query and define its matching `LiveQuery`. One filter or demand is the one-branch case; use several branches when the feature genuinely unions distinct demands, and read `RowBatch.evidence` by branch index.
2. Say nothing about routing unless you mean to override NMP. A `Demand` that names only its selection gets `ReadRouting::Auto`: NIP-65 outbound relays for every author the selection resolves, relay hints and prior provenance, then the operator's app and fallback lanes. `Auto` is total — an authorless selection is the same path with no authors to solve for, not a different one — so there is nothing to choose and no routing error to handle. Name `ReadRouting::Explicit(relays)` only when the app genuinely means "ask these and nothing else"; it is never widened, and an empty set is refused at construction. A non-default `access` (NIP-42) and strict cache provenance over an `Explicit` set are separate axes, set independently.
3. Start one observation at the feature/lifecycle owner. Rust consumers accumulate deltas by id; Swift/Kotlin replace from each already-accumulated `RowBatch` snapshot. Render app-owned order.
4. Render acquisition evidence and shortfalls as facts, not a global verdict.
5. If the feature writes, construct one `WriteIntent`, retain the receipt, and model per-relay outcomes.
6. Bind query, signer-session, engine teardown, and fact-stream consumption to deterministic owners. There is no worker/task admission ceiling: observers, actions, and signers run as async tasks on one shared engine-owned runtime, so ordinary concurrent operations just make progress. Preserve the genuine infra outcomes (`EngineStartFailed` at construction, `ObservationUnavailable` for a live observe) and terminal action statuses as distinct facts at their owning boundaries. Detaching a receipt fact stream is distinct from cancelling the write: `ReceiptStatus.cancel()` / ending the Kotlin collection scope stops delivery, while `NMPEngine.cancel(receiptId:)` ends a not-yet-signed obligation.
7. Add a bounded running proof using a real or scripted relay. Include restart proof for durable receipts or persistent cache claims.

## Review checklist

- Does every API name exist for the selected platform *and* the capability set the app's `.nmp.toml` actually selects?
- Does any target/internal behavior masquerade as current public behavior?
- Is relay authority explicit where the default is unsuitable?
- Are rows accumulated from the SDK stream instead of mirrored from write intent?
- Can every observation and connection be cancelled promptly?
- Does the delivery UI keep the `WriteFact.destinations(relays:complete:awaitingAuthorRoutes:)` `complete` flag separate from delivery, and preserve per-relay `RelayState` `.rejected`/`.authFailed`/`.gaveUp` rather than collapsing them? "Still determining where to send" and "nowhere to send" are the two sides of that one branch, not one sentence.
- Are identity persistence and destructive store reset separate operations?
- Are secrets absent from logs and source?
