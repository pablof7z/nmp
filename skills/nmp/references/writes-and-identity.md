# Writes and identity

## Write intent and receipt

A `WriteIntent` has exactly three parts: payload, routing, and identity.

- Payload is `WritePayload`, three arms: `Event(EventBuilder)`, `ReplaceableOperation(..)`, and `Signed(Event)`. The builder demands only a kind; `created_at`, the author, the id and the signature are filled in at acceptance when you did not say them, and stating a `created_at` keeps it verbatim. The *builder* carries no author. `ReplaceableOperation` is an opaque value minted by an engine-issued registration handle — apps never construct one, and `publish` rejects an unknown registration before custody; replaceable operations are reached through protocol actions such as following, not a raw payload constructor.
- Routing is two words: `Auto`, or `Explicit(relays)` which is honoured verbatim and never widened. An empty explicit set is refused before acceptance. Publishing to a chosen relay is a first-class general capability, not a protocol-module privilege.
- Identity is `Active` (default) or `Explicit(PublicKey)`. `Explicit` publishes as a held secondary key without touching the active account, and works while logged out.

There is no durability or lane choice. Every accepted write takes durable custody.

Publishing enqueues work and returns a fact stream. It does not mean all relays accepted the event. `Ok` from `publish` *is* acceptance — there is no separate bridge established before it, and no capacity or thread refusal on the path.

When a workflow genuinely needs the terminal answer rather than the running facts, `ReceiptStream::result()` awaits it and hands back a `ReceiptResult { outcome, relays }`: the one `WriteOutcome` plus the last recorded state of every destination, so a mixed publish/reject never collapses into a Boolean. NMP owns that reduction and its durable replay; apps do not fold the fact stream themselves. It fails only as `ReceiptResultError::ClosedWithoutOutcome` or `ReplayUnavailable`.

## The custody vocabulary

`WriteFact` has four arms, and only one of them ends anything:

- `Signing(SigningState)` — whole-write, one signature: `AwaitingSigner { pubkey }`, `InFlight { pubkey }`, `Signed { event_id }`, `Refused { reason }`. `AwaitingSigner` and `InFlight` are the two an app must never merge: `InFlight` is the ordinary state of every healthy write between acceptance and signature and is no reason to trouble a user, while `AwaitingSigner` is the genuinely parked one whose only other exit is the app cancelling it. Collapse them and the parked write becomes impossible to pick out.
- `Relay { event_id, relay, state }` — per-relay: `Waiting(RelayWaiting)`, `Sent { attempt, written_at }`, `Published`, `Rejected { reason }`, `AuthFailed { pubkey, source, reason }`, `GaveUp`. `event_id` names the exact immutable bytes this evidence is about: one stable receipt can span several successor generations, so the receipt id alone cannot identify them. `RelayState::is_terminal()` answers "will this relay produce another fact" without the app re-deriving it.
- `Destinations { relays, complete, awaiting_author_routes }` — where the write is intended to go, and whether that picture is settled. `complete` flips on settled resolution, never on delivery, so `complete: true` with nothing published yet is an ordinary state. `awaiting_author_routes` names the public keys resolution is still waiting on, so an app can say who it is waiting for.
- `Outcome(WriteOutcome)` — the only whole-write terminal: `Settled`, `NoDestination`, `NotSent(NotSentReason)`, `Superseded`, `Refused(RefuseReason)`. `NotSentReason` is `Cancelled`, `SignerRefused`, or `Superseded`, and every one of them means NMP proved the bytes never crossed the local transport handoff. Top-level `Superseded` is the honest opposite: a newer replaceable write retired this obligation *after* the bytes may already have gone, so NMP will not retry it but does not claim it was never sent.

`RelayWaiting` is `NotConnected`, `NeedsAuth`, `Eligible { since }`, or `BackingOff { attempt, eligible_at, cause, detail }`. There is no arm for a local-store failure: a durable-store write that fails costs the lane's progress and nothing else, so it produces no relay state, no fact, and nothing to render.

Treat these as evidence, not commands. `BackingOff` carries the engine-owned durable scheduler's persisted attempt ordinal, deadline, and a typed `RetryCause`; it does not grant an app retry verb. `Sent` means transport proved socket write and flush for that persisted attempt — it is not an ack and it is not terminal.

## Cancelling, enumerating, and recovering

- `Engine::cancel(receipt_id)` cancels a write that has not yet been signed and commits `WriteOutcome::NotSent(NotSentReason::Cancelled)`. Past that point it refuses with a typed `CancelWriteError`: `AlreadySigned`, `AlreadyCompensated`, `AlreadySuperseded`, `AlreadyRefused`. This is distinct from detaching a live fact stream.
- `Engine::publish_queue(after, limit)` reads retained writes back as `PublishQueueEntry` values with their full current state: frozen `event_id` and `pubkey`, `accepted_at`, `signing`, the intended `relays` set with `route_complete`, per-relay `relay_states`, and `outcome` once the whole write ended. It is paged, not an enumeration of everything: `after` is an exclusive stable receipt-id cursor and `limit` is a `u8`, so one call can never materialize more than 255 entries. It is inspection and never blocks on settlement. `Engine::publish_queue_for_event(event_id, after, limit)` is the join from a live-query row's event id to the receipt ids currently open for it; more than one receipt can own identical bytes, so it too is bounded and paged rather than picking one. `remove_publish_queue_entry` is the companion, and the pair is a termination path rather than housekeeping: a write parked on an unavailable signing provider, and a permanently-failed entry, end only by the app's own decision — cancel the parked one, then remove whichever terminal entry is left.
- App-controlled retry does not exist. Retry belongs entirely to the engine-owned durable scheduler and surfaces only as `RelayWaiting::BackingOff`. Do not invent retry buttons that call an absent API. A product may let the user compose a new intent, but that is a new publication decision, not a retry of the same obligation.

Recovery has sharp edges:

- A refusal *before* acceptance takes nothing into custody. No receipt, no stream, and no queue entry exist for it — you get a typed error, not an id. `publish` refuses in exactly two classes: NMP cannot return an accepted answer (the engine is draining, the receipt-id space is exhausted, or the acceptance transaction reports persistence failure), or the instruction cannot resolve (an unverifiable supplied signature, an `Identity::Active` write with no current account, an explicit identity contradicting a signed payload's own author). An acceptance I/O error is the deliberate ambiguity: the call returns no id, but durability is unknown until reconstruction, and paging the publish queue after restart may reveal one fully committed pending row. Facts about viability *after* acceptance — no relays or no available signing provider, and I/O failures during later signing/delivery work — stay in custody and fail or park in the queue where the app can see them. Replayable `ReplaceableOperation` capabilities retain the operation and reapply it over newer source truth. NIP-02 uses that path and owns an explicit complete empty first value, so apps do not implement a stale-base retry. The `RefuseReason` arms are `AlreadyExpired` and `Tombstoned`.
- Reattachment returns `Attached`/`NotFound`/`RetainedButUnreadable` and traverses the durable `WriteFact` history in finite pages before streaming onward. Lag is the typed `FactStreamLagged`, not silent loss.
- Reattach by id with `reattach_receipt`. Between that door and paging `publish_queue`, an app that crashed after acceptance can find its outstanding writes again.
- NIP-22 composes an ordinary `WriteIntent` and uses the generic publish path. NIP-29's `Group::publish` mints its intent privately and returns the same ordinary `ReceiptStream` every other write returns. Neither has a composed carrier or a second publication lifecycle.
- Restore the whole session so accepted unsigned work can resume when the frozen account's provider is available. Fact-stream closure alone is never delivery success; retain the mixed facts already observed.

Dropping the `ReceiptStream` (its `statuses` receiver) stops delivering live frames to that stream and leaves the durable receipt untouched — stream detachment, not write cancellation. `Engine::cancel(id)` is the door that ends the obligation. The live fact channel is finite and reports `FactStreamLagged` rather than growing without bound.

## Identity

Adding an account and making it current are separate operations. Changing the current account re-roots reactive identity bindings and every `Identity::Active` write not yet accepted; acceptance pins the identity so a later switch cannot retarget an accepted write.

Publishing with `Identity::Active` and no current account is refused *before* acceptance — that is a typed error, not receipt evidence. Publishing under a current key whose configured provider is unavailable, or under a public-key-only account, is accepted and parks durably as `SigningState::AwaitingSigner { pubkey }`; parking is not failure, and no clock ends it. Ending such a write is the app's decision: `cancel`, then `remove_publish_queue_entry`.

The public session API adds local-key-backed or public-key-only accounts, makes one current, removes one whole account, clears the session, and exports one opaque payload. Clearing the session drops accounts, providers and the current selection and leaves cached events, receipts and accepted write obligations alone. An account handle carries its persisted provider kind and, separately, `signing: SigningAvailability` — availability is deliberately never inferred from the provider kind, and provider reachability never removes the account.

Sign-only is separate from publication. `Engine::sign_event(SignEventRequest)` returns the cancellable `SignEventOperation`. NMP freezes the active author before asynchronous work and verifies the exact returned event. Success creates no write intent, pending row, receipt, stored event, route, relay attempt, or publication claim. An asynchronous signer resolves through the opaque `PendingSignerSender` returned by `SignerOp::pending_channel` or `pending_channel_with_cancel`; its internal receiver is not public API.

The app stores the opaque whole-session payload as a single sensitive value and hands it back at engine construction — there is no separate import call — and NMP ships neither a platform-vault adapter nor a plaintext credential checkpoint. App-owned transactional storage is tracked in #1398.

## Reset is destructive

`Engine::reset_persistent_store(path)` removes NMP's canonical events, pending writes, receipts, coverage, and evidence at that path. Shut down and drop all engines using the path first — a live engine on the same canonical path, in this or any other process, is refused with `EngineError::StoreStillOpen` and the file is left alone. A missing path is already reset and succeeds. Reset does not clear an app-stored session payload; logout flows must treat those as distinct authorities.
