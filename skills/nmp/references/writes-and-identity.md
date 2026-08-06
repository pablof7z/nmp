# Writes and identity

## Write intent and receipt

A `WriteIntent` has exactly four parts: payload, routing, identity, and an optional correlation token.

- Payload: an `EventBuilder`, a compare-and-swap replaceable edit built from one, or an already signed event. The builder demands only a kind; `created_at`, the author, the id and the signature are filled in at acceptance when you did not say them, and stating a `created_at` keeps it verbatim. The *builder* carries no author. Swift/Kotlin ergonomic payloads expose the builder and signed forms; native replaceable edits are reached through governed protocol actions such as following, not a raw payload constructor.
- Routing is two words: `Auto`, or `Explicit(relays)` which is honoured verbatim and never widened. An empty explicit set is refused before acceptance. Publishing to a chosen relay is a first-class general capability, not a protocol-module privilege.
- Identity is `Active` (default) or `Explicit(PublicKey)`. `Explicit` publishes as a held secondary key without touching the active account, and works while logged out.
- `correlation` is a caller-minted token journaled inside the same acceptance transaction. Persist it *before* publishing and you can recover the receipt after a crash that happened before you stored the id.

There is no durability or lane choice. Every accepted write takes durable custody.

Publishing enqueues work and returns a fact stream. It does not mean all relays accepted the event. `Ok` from `publish` *is* acceptance — there is no separate bridge established before it, and no capacity or thread refusal on the path.

## The custody vocabulary

`WriteFact` has four arms, and only one of them ends anything:

- `Signing(SigningState)` — whole-write, one signature: `AwaitingSigner { pubkey }`, `InFlight { pubkey }`, `Signed { event_id }`, `Refused { reason }`.
- `Relay { relay, state }` — per-relay: `Waiting(RelayWaiting)`, `Sent { attempt, written_at }`, `Published`, `Rejected { reason }`, `AuthFailed { pubkey, source, reason }`, `GaveUp`.
- `Destinations { relays, complete, awaiting_author_routes }` — where the write is intended to go, and whether that picture is settled. `complete` flips on settled resolution, never on delivery, so `complete: true` with nothing published yet is an ordinary state. `awaiting_author_routes` names the public keys resolution is still waiting on, so an app can say who it is waiting for.
- `Outcome(WriteOutcome)` — the only whole-write terminal: `Settled`, `NoDestination`, `NotSent(Cancelled|Superseded)`, `Refused(RefuseReason)`.

`RelayWaiting` is `NotConnected`, `NeedsAuth`, `BackingOff { attempt, eligible_at, cause, detail }`, or `PersistenceStalled { detail }`.

Treat these as evidence, not commands. `BackingOff` carries the engine-owned durable scheduler's persisted attempt ordinal, deadline, and a typed `RetryCause`; it does not grant an app retry verb. `Sent` means transport proved socket write and flush for that persisted attempt — it is not an ack and it is not terminal.

## Cancelling, enumerating, and recovering

- `Engine::cancel(receipt_id)` — Swift `cancel(receiptId:)`, Kotlin `cancel(receiptId)` — cancels a write that has not yet been signed and commits `WriteOutcome::NotSent(NotSentReason::Cancelled)`. Past that point it refuses with a typed `CancelWriteError`: `AlreadySigned`, `AlreadyCompensated`, `AlreadySuperseded`, `AlreadyRefused`. This is distinct from detaching a live fact stream.
- `Engine::publish_queue()` — Swift/Kotlin `publishQueue()` — enumerates every retained write as a `PublishQueueEntry` with its full current state, including a latched `persistence_fault`. `remove_publish_queue_entry` is the companion, and the pair is a termination path rather than housekeeping: a write parked on a missing signer, and a permanently-failed entry, end only by the app's own decision — cancel the parked one, then remove whichever terminal entry is left.
- App-controlled retry does not exist on any tier. Retry belongs entirely to the engine-owned durable scheduler and surfaces only as `RelayWaiting::BackingOff`. Do not invent retry buttons that call an absent API. A product may let the user compose a new intent, but that is a new publication decision, not a retry of the same obligation.

Recovery has sharp edges:

- A refusal *before* acceptance takes nothing into custody. No receipt, no stream, and no queue entry exist for it — you get a typed error, not an id. A stale replaceable base is not pre-acceptance: it takes custody and becomes a readable `Refused(RefuseReason::ReplaceableBaseChanged { expected, actual })` entry, which keeps both ids deliberately so the app can refetch `actual`, reapply, and resubmit silently.
- Reattachment returns `Attached`/`NotFound`/`RetainedButUnreadable` and traverses the durable `WriteFact` history in finite pages before streaming onward. Lag is the typed `FactStreamLagged`, not silent loss.
- Reattach by id, or by the correlation token you persisted before publishing. Between the two doors plus `publishQueue()`, an app that crashed after acceptance can find its outstanding writes again.
- NIP-22 composes an ordinary `WriteIntent` and uses the generic publish path. NIP-29's `Group::publish` mints its intent privately and returns the same ordinary `ReceiptStream` every other write returns. Neither has a composed carrier or a second publication lifecycle.
- Restore the signer and active account so accepted unsigned work can resume. Fact-stream closure alone is never delivery success; retain the mixed facts already observed.

Swift `ReceiptStatus.cancel()` stops delivering live frames to that stream and leaves the durable receipt untouched; Kotlin's status `Flow` is a cold pull loop that cancels the underlying stream when the collection scope ends. Both are stream detachment, not write cancellation — `NMPEngine.cancel(receiptId:)` is the door that ends the obligation. The live fact channel is finite and reports `FactStreamLagged` rather than growing without bound.

## Identity

Adding a local account and activating it are separate operations. Changing the active account re-roots reactive identity bindings and every `Identity::Active` write not yet accepted; acceptance pins the identity so a later switch cannot retarget an accepted write.

Publishing with `Identity::Active` and no active account is refused *before* acceptance — that is a typed error, not receipt evidence. Publishing under an active key for which no signer is installed is accepted and parks durably as `SigningState::AwaitingSigner { pubkey }`; parking is not failure, and no clock ends it. Ending such a write is the app's decision: `cancel`, then `removePublishQueueEntry`.

Direct Rust can register an arbitrary `SigningCapability` through `add_signer`. Swift/Kotlin expose local-key account import, not arbitrary Rust trait implementations.

Governed sign-only is separate from publication. Direct Rust calls `Engine::sign_event(SignEventRequest)` and owns the returned cancellable `SignEventOperation`; Swift calls async `signEvent(NMPUnsignedEvent)` and Kotlin calls the suspending equivalent. NMP freezes the active author before asynchronous work and verifies the exact returned event. Success creates no write intent, pending row, receipt, stored event, route, relay attempt, or publication claim. A direct-Rust asynchronous signer resolves through the opaque `PendingSignerSender` returned by `SignerOp::pending_channel` or `pending_channel_with_cancel`; its internal receiver is not public API.

Swift and Kotlin each ship a genuine secure account store — `NMPKeychainAccountStore` over `SecItem`, and a `java.security.KeyStore`-backed store — alongside the explicitly insecure plaintext file stores. All satisfy the same `NMPLocalAccountCheckpoint` contract, so the engine restores from whichever conformer the app injects. Prefer a platform-vault conformer; reach for the file store only for development. The Kotlin secure store is desktop-JVM `KeyStore`, explicitly not AndroidKeyStore. Clear persisted credentials on sign-out.

## Reset is destructive

Persistent-store reset removes NMP's canonical events, pending writes, receipts, coverage, and evidence at that path. Shut down and drop all engines using the path first. It does not clear separately configured account/signer persistence; logout flows must treat those as distinct stores.
