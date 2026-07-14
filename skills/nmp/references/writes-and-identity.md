# Writes and identity

## Write intent and receipt

A `WriteIntent` combines payload, durability, and routing.

- Direct Rust payload: unsigned template, compare-and-swap unsigned replaceable edit, or already signed event. Swift/Kotlin ergonomic payloads expose only unsigned and signed; native replaceable edits are reached through governed protocol actions such as following, not a raw payload constructor.
- Durability: durable, ephemeral, or at-most-once.
- Consumer-constructible routing is author outbox or recipient inboxes. Fail-closed private routes and pinned hosts are withheld from the default facade and reached only through trusted protocol composition where implemented.

Publishing enqueues work and returns an evidence stream. It does not mean all relays accepted the event. Handle every current terminal or ambiguous state listed in [Current surface and gaps](current-surface.md), especially per-relay rejection, `GaveUp`, persistence blockage, `OutcomeUnknown`, replaceable conflict, and whole-intent failure.

Use tracked/native receipt publishing when the app must survive process loss. Persist the receipt id outside transient UI state, reopen the same NMP store, and call receipt reattachment. Distinguish attached, not found, and retained-but-unreadable.

Recovery has sharp edges:

- A pre-acceptance conflict or failure can return a stream-local correlation id with no durable receipt row; reattaching that id returns not found.
- Reattachment replays retained receipt state, terminal attempt outcomes, and current persistence-blocked facts. It does not reconstruct transient `Routed` or `Sent` history, so journal live statuses if the product needs that history after restart.
- NMP exposes no receipt enumeration. Persist the id as soon as it is returned, but acknowledge the crash window after engine acceptance and before app persistence.
- Native tracked/composed publish reserves and starts its receipt observer before core acceptance, and composed publish does so before taking its intent. `ExecutorSaturated` or `ThreadUnavailable` therefore returns synchronously without accepting an obligation, consuming that composed intent, or returning an id.
- Restore the signer and active account so accepted unsigned work can resume. Receipt-channel closure alone is never delivery success; retain the mixed terminal facts already observed.

Swift/Kotlin `Receipt` has no cancel/detach handle. Cancelling the task or collector stops app consumption; it does not demonstrably detach the native receipt observer, and it does not cancel the engine-owned obligation. The bridge remains until its channel/engine closes; Kotlin's channel is unbounded, so keep one owned collector draining and avoid repeated attachment to the same id. A production native app must restore its secret through app-owned secure storage, call account add and activation again, then reattach receipts. Only the explicitly insecure file account store performs automatic credential restoration.

There is no public cancel-write or app-controlled retry method. Do not invent retry buttons that call an absent API. A product may let the user compose a new intent, but that is a new publication decision and must not be described as retrying the same obligation.

## Identity

Adding a local account and activating it are separate operations. Changing the active account re-roots reactive identity bindings and unsigned writes. Read-only browsing may activate a public key for which no signer is installed; publishing then fails through receipt evidence.

Direct Rust can register an arbitrary `SigningCapability`. Swift/Kotlin expose local-key account import and NIP-46 connection helpers, not arbitrary Rust trait implementations.

Governed sign-only is separate from publication. Direct Rust calls `Engine::sign_event(SignEventRequest)` and owns the returned cancellable `SignEventOperation`; Swift calls async `signEvent(NMPUnsignedEvent)` and Kotlin calls the suspending equivalent. NMP freezes the active author, admits bounded work before invoking the signer, and verifies the exact returned event. Success creates no write intent, pending row, receipt, stored event, route, relay attempt, or publication claim. A direct-Rust asynchronous signer resolves through the opaque `PendingSignerSender` returned by `SignerOp::pending_channel` or `pending_channel_with_cancel`; its internal receiver is not public API.

NIP-46 handoff readiness is asynchronous. Derive/cache the handoff URI or value before invitation connection consumes the invitation; then connect/start listening, launch the cached handoff, and wait for the connection state to become ready. OS launch success is not signer readiness. Close the exact connection deterministically.

The optional `NMPInsecureFileAccountStore`/`NMPInsecureFileAccountStore` equivalents persist plaintext credentials for explicitly insecure personal/development use. They are not Keychain, Secure Enclave, or Keystore integrations. Clear persisted credentials before shutting down on sign-out.

## Reset is destructive

Persistent-store reset removes NMP's canonical events, pending writes, receipts, coverage, and evidence at that path. Shut down and drop all engines using the path first. It does not clear separately configured account/signer persistence; logout flows must treat those as distinct stores.
