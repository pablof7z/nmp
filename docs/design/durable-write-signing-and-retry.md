# Durable writes, signing, and retry

- **Status:** IMPLEMENTED - crash-safe acceptance, canonical pending rows,
  signer reattachment, the one durable retry scheduler, and truthful governed
  lane-state projection in the `nmp` facade satisfy this contract.
- **Owns:** the meaning of `Accepted`, pending-row semantics, signer selection,
  receipt persistence, retry ownership, and bounded progress when read and
  write authenticated identities compete for physical relay sessions.

## 1. Acceptance transaction

For a durable write, `Accepted` is emitted only after one atomic persistence
boundary records:

- the frozen unsigned NIP-01 body and expected author pubkey;
- the stable event id derived from that body;
- the durable intent and receipt identity;
- signature state `Pending(intentId)`;
- the canonical pending row inserted through the ordinary event-store mutation
  path;
- any displaced replaceable winner needed for pre-signature compensation;
- any older, unattempted delivery obligations made obsolete by this
  replaceable/addressable winner, retired with retained receipt facts;
- initial route/retry state that is already known.

If the call returns an error, the caller receives no `Accepted` answer. A commit
that fails has unknown durability: boot recovery or publish-queue enumeration may
still reveal one pending row it committed. NMP branches on nothing here — it does
not classify the failure, does not reconcile it, and does not repeat the
operation. `Accepted` never means merely queued in memory.

### Replaceable delivery coalescing

Acceptance uses the same NIP-01 coordinate as canonical replacement:
`(pubkey, kind)` for kinds `0`, `3`, and `10000...19999`, and
`(pubkey, kind, d)` for `30000...39999`. When a newer winner is accepted,
every older open intent that owns the displaced row is retired in the same
transaction. Its event body, intent, waiting lanes, attempt rows, route
revisions, and deadlines are removed. Work proved never handed off leaves no
receipt or correlation identity. Work that may have crossed the local
transport handoff retains only terminal `Superseded` safety evidence, which is
retained as one ordinary terminal closure. It competes in the same store-owned
oldest-first history as acknowledgements, refusals, cancellations, and
no-destination outcomes; there is no supersession-specific retention class.
While retained, its complete safety evidence remains available. Whole-closure
eviction is bounded privately by age, count, and logical encoded bytes.

An offline or AUTH-blocked lane with attempt ordinal zero is still unattempted:
route resolution alone is not evidence that bytes reached a relay, so its
receipt is destroyed too. Every attempt record is decoded and validated in
full. Explicit proof that it was not handed off is still safely disposable;
missing, possible, or actual handoff evidence preserves only the bounded
terminal uncertainty fact. The old attempt cannot retry and its bytes cannot
re-enter the queue. Co-owners are classified independently when deciding
whether any safety receipt is justified.

Compensation chains remain valid only for relay-observed canonical state. A
predecessor with independent relay provenance may stay stashed under the newer
intent because it is cache truth NMP did not invent. A superseded local
attempt—signed or unsigned, attempted or not—is obsolete and never restorable.
Cancelling the newer write can therefore never resurrect unpublished local
history.

## 2. One row path

The pending row participates in ordinary filters, derived bindings,
replaceable/delete/expiry semantics, persistence, GC claims, and query
invalidation. The write path has no direct observer callback and no optimistic
overlay.

NIP-01 event identity excludes the signature, so the id does not change when a
signature arrives. A valid signature atomically promotes the same row:

```text
Pending(intentId) -> Signed(signature)
```

The returned signed event must match the frozen body and expected pubkey exactly
and must verify cryptographically before promotion.

Cancellation or terminal pre-signature protocol failure removes the pending row
through the ordinary store door. If it displaced a replaceable winner, the
engine offers that prior row back through the same door as a compensating
mutation. After signature promotion, relay ACK/rejection changes receipt state
only; it never retracts the valid signed event.

## 3. Signer selection and reattachment

The ergonomic default is the signer registered for `$currentPubkey`:

```text
publish(draft)
publish(draft, as: identityRef)  // exceptional override
```

The override supports podcast identities, disposable identities, delegation,
hardware keys, and similar cases without making them globally active. The app
does not need to retain or pass a signer object on ordinary writes.

Before acceptance NMP resolves a stable expected author identity. At acceptance
that identity is pinned. A later current-pubkey change cannot redirect the
intent to another signer.

If the matching capability is absent or temporarily offline, the receipt says
`AwaitingSigner(pubkey)`. The durable obligation remains until the app attaches
a matching signer, explicitly cancels it, a terminal protocol failure occurs,
or protocol expiry makes it invalid. Missing NIP-46 connectivity is not failure.

### Governed sign-only operation

Signing and publishing are orthogonal. A host that must authorize an external
client's exact Nostr event uses the engine's sign-only operation rather than
fabricating an ephemeral write intent.

The request carries an immutable unsigned NIP-01 body whose author must equal
the current session account. Acceptance freezes that author and body, resolves only the
matching registered capability, and admits pending signer work through the
same finite native-task owner used by other signer requests. The returned event
is released only after its body, author, computed id, and signature all
validate. Cancellation is scoped to that one signer operation.

This path deliberately bypasses write acceptance. It creates no canonical
pending row, intent or receipt id, delivery journal/lane, relay plan, or
publication. NIP-07 origin authorization and prompting remain host policy; the
operation supplies governed key custody and exact-result validation only.

## 4. Secret-material boundary

The Rust event/delivery store persists signing obligations, expected pubkeys,
frozen bodies, and validated signatures. It does not persist raw secret keys.

The session owns which accounts exist, each account's optional persistable
provider configuration, and the optional current selection. It exports and
restores those facts as one opaque sensitive value; the app owns storing that
value plus identity import/removal/backup UX.

Provider availability is operational state, not a second kind of membership.
A remote, hardware, or callback-backed provider may be unreachable after
restore while its account remains known. An accepted intent then remains
`AwaitingSigner` for its frozen public key until that provider becomes
available or the app cancels it; NMP must not discard or re-author it.

## 5. Receipt durability

Receipt facts are persisted and reattachable by intent/receipt id. Dropping an
observer does not cancel the write or lose its history. `Accepted`, signer
waiting, signature promotion, replaceable supersession, route revisions,
attempts, ACKs, rejections, exact-session authentication denials, expiry,
cancellation, and ambiguous at-most-once outcomes remain inspectable after
restart.

The canonical facade operation is `cancel(receipt_id)`. It commits only for a
still-unsigned accepted obligation, returns `CancelWriteOutcome::Cancelled`,
persists and broadcasts the matching `WriteStatus::Cancelled` fact, and is
idempotent once that fact exists. Unknown ids, signed writes, superseded
writes, and each other terminal state are distinct typed refusals. Store
failure is a typed error: ownership and signer work remain live, and no
observer sees `Cancelled` unless the compensation transaction committed.

`Enqueued`, `sent`, and `converged` are never synonyms. Product policy may
interpret a set of per-relay facts; the engine reports them without inventing a
single success boolean.

`Sent { relay, attempt, written_at }` is constructible only from a persisted
`Written` handoff for that exact durable lane ordinal. Ephemeral transport work
has no delivery attempt and therefore cannot mint this durable receipt fact.

## 6. Retry ownership

Retry is split by domain, with exactly one owner each:

| Domain | Owner | Durable responsibility |
|---|---|---|
| Socket connection | transport | reconnect the socket; never buffer durable EVENTs invisibly |
| One remote-sign request | signer adapter | correlation, AUTH/connect for that operation, exact response validation |
| One `(intent, relay)` lane | publish queue | attempt state, eligibility, terminal relay evidence |
| Time and concurrency | engine deadline scheduler | wake eligible work without poll loops or per-intent threads |

For every durable relay lane the delivery store persists the exact signed bytes,
`AttemptStarted`, attempt ordinal, outcome, and `nextEligibleAt`. Backoff uses
deterministic jitter and explicit caps so restart does not reset or synchronize
the fleet.

- Offline and AUTH-blocked time do not consume attempts.
- `AuthRequired` is a resumable `AwaitingAuth` state, never a retry outcome.
  It arms no EVENT deadline and is absent from the public `RetryCause` type.
- Only three exact answers can terminalize an authenticated write lane:
  `AuthPolicyDecision::Deny`, `SignerError::Rejected` while signing its
  kind:22242 challenge, and an `OK false` correlated to that exact AUTH event.
  Policy execution errors, unavailable signers, and subscription `CLOSED`
  auth-required/restricted frames do not have that authority.
- An authentication denial first commits
  `PublishQueueTerminalOutcome::AuthDenied { source, reason }` against the lane's
  exact expected revision, then emits `WriteStatus::AuthDenied`. Idempotent
  success is considered only after that revision check, so a stale caller
  cannot mistake a newer equal-looking terminal fact for its own transition.
  Persistence failure emits no terminal receipt fact.
- A newer same-coordinate winner retires an older lane only while its attempt
  ordinal is still zero; attempted delivery remains owned until terminal.
- Recovery wakes work whose persisted eligibility time has passed.
- A transient delivery failure advances backoff and replays its exact
  persisted non-AUTH `RetryCause` plus optional relay detail.
- A relay ACK closes its lane.
- A route revision may add a new lane without reopening completed lanes.
- A permanent relay rejection is terminal evidence for that lane, not row
  retraction.
- At-most-once ambiguity becomes `OutcomeUnknown`; it is never blindly retried.

There is no fixed-rate polling. The scheduler sleeps until the earliest real
deadline and rearms after every state transition.

### A store failure loses progress, never an accepted write

There is no degraded mode, no latched fault, no reopen, and no classification
of local disk failure. A store operation that fails returns
`PersistenceError` — opaque, carrying only the backend's message — the caller
propagates it, and the engine carries on with the same handle. Nothing branches
on the kind of failure, because nothing needs to: the cost of losing a durable
write is bounded by where acceptance commits.

`accept_write` commits the intent, the receipt, the frozen body and the
canonical pending row in **one** transaction, and `publish()` returning `Ok` is
constructible only after that commit. So a store failure can destroy progress —
which relays a write reached, how far a lane got — and boot recovery rebuilds
that from the durable rows the acceptance transaction already holds. It cannot
destroy the obligation itself. Publish while offline, quit, reopen, and it
sends.

Progress that fails to commit stays *unfinished*, never *finished-as-nothing*.
A route revision whose commit fails leaves `pending.route_complete` false:
routing is not complete when nothing durable holds the answer, so the next pass
resolves and commits again rather than letting an empty durable route set read
as the terminal `NoDestination` verdict.

The cross-process ownership fence is unaffected — it just no longer needs a
reopen-while-owned path.

Falsifiers:
`nmp-engine::a_failed_lane_attempt_commit_loses_progress_and_a_fresh_engine_resumes_the_write`
and
`nmp-engine::a_failed_route_revision_commit_loses_progress_and_a_fresh_engine_resumes_the_write`
(a real redb file takes a real post-acceptance commit failure at each of the
two durable progress boundaries; no app-facing fact is emitted, the engine
keeps serving, and a fresh engine over the same file recovers the receipt, its
frozen bytes and its route set and sends the write), and
`nmp::persistent_engine_keeps_healthy_store_usable_after_invariant_fault` (real
targeted canonical corruption refuses that exact publish and the next write
uses the same healthy Redb handle).

### Recovery costs what changed, not what accumulated

The engine thread rebuilds volatile ownership from the durable queue before it
reads its first command, so recovery's cost is what the app's first call pays.
The bound is therefore stated on the work rather than on a clock: **reopening
commits one durable transaction per lane fact that is not already durable, and
none at all for a queue that has not changed** (#889).

Two paths used to violate it, and both spent a durability barrier to leave the
database byte-identical:

- lane bootstrap is idempotent and runs once per open intent on every boot, so
  the overwhelmingly common call finds a complete lane set. It now commits only
  when it stages a row, and aborts the transaction otherwise.
- connectivity is process-local. An `Eligible` lane whose session is absent and
  a `WaitingConnection` lane read back as the identical
  `RelayState::Waiting(NotConnected)` through the enumeration door, so
  re-parking an eligible lane whose relay is merely not connected — which at
  boot is every eligible lane, because nothing is connected yet — recorded
  nothing a later boot or an app could observe. The lane is left alone; the same
  scheduler pass that closes every relay wake picks it up when a session exists.

  The equivalence holds only for the absent-session half, and it holds because
  the enumeration door asks `connected_relays` the same question the scheduler
  does. An `Eligible` lane that DOES have a live session projects as
  `RelayState::Waiting(Eligible { since })`: it is queued behind the relay's one
  attempt slot, nothing is wrong with its connection, and telling an app
  otherwise invents a fault. Widening this bullet back into "`Eligible` and
  `WaitingConnection` are the same answer" would restore that lie.

The bound exists because of an incident where boot recovery on a large store,
every lane eligible and unreached, was slow enough to block `add_account`
behind it.

The before/after wall-clock pair that used to appear here is deleted. Its
"before" side named no base commit at all, it was a single unpaired sample with
no host recorded, and no result file was committed — so the magnitude cannot be
re-established and should not be cited. The rule it justified does not depend
on it: that recovery commits one durable transaction per CHANGED lane fact and
none for an unchanged queue is proven structurally by the two falsifiers below,
which pass today. The on-demand `nmp::measure_add_account_behind_boot_recovery`
(`crates/nmp/tests/boot_recovery_bound.rs`, `#[ignore]`, 4,000-intent fixture)
remains available to produce a current number for anyone who needs one.

Falsifiers: `nmp::boot_recovery_rewrites_no_lane_when_no_durable_fact_changed`
(no lane revision moves across a reopen),
`nmp-store::a_lane_bootstrap_that_stages_no_row_commits_nothing` (the unstaged
bootstrap count is the whole population).

Recovery still visits every open intent, so what remains linear is bounded
reads, not durability barriers. The other half of keeping that number small is
acceptance-time retirement: a replaceable/addressable winner retires older
obligations at its address that never started a wire attempt, so a presence
renewal loop against an unreachable relay leaves one obligation rather than one
per renewal (`nmp::presence_renewals_leave_exactly_one_open_obligation`).

### Access-scoped sessions under the physical cap

A relay URL does not imply one interchangeable socket. Public reads and
identity-scoped `Nip42(author)` work are distinct `RelaySessionKey`s and never
share authentication state. `max_relays` is nevertheless a ceiling on physical
sessions, not on distinct URLs. At a ceiling of one, a live Public read and a
durable write to that same relay therefore cannot coexist.

The same identity rule governs denial. A terminal AUTH decision applies only
to lanes whose `RelaySessionKey` exactly equals the challenged
`(relay, Nip42(pubkey))`; another identity on the same URL remains live.
Read-side subscription closure is not a write-session decision and cannot
terminalize any write lane.

The reducer makes the scheduling authority explicit:

- read demand emits `EnsureReadRelay`; it cannot displace another live session;
- nonterminal write ownership emits `EnsureWriteRelay`; only that effect may
  release the same relay's Public session and claim its slot;
- a protected read does not gain write priority merely because it also uses a
  identity-bound session;
- no admission path evicts a different relay or raises the physical-session
  ceiling.

Releasing the Public session does not withdraw its query demand or erase its
reconnect preamble. The ordinary reducer receives the exact closed-session
fact, the write's access-scoped worker runs through its normal AUTH and
delivery path, and terminal write reconciliation releases it. The next real
worker retirement restores any still-required Public session; the reducer
replays its current request set once after the fresh Connected edge. Retry
ordering derives from one coherent reducer snapshot whose `writes` set is a
typed subset of the exact retained worker set.

This is bounded time-sharing, not socket-context coalescing and not public
saturation. It closes the `max_relays = 1` deadlock where the Public read could
hold the only slot forever while its own discovered route left the durable
write parked at `AwaitingRelay` (#598).

## 7. Falsification

Required proofs include:

- crash immediately after `Accepted` restores the pending row and receipt;
- matching queries and derived bindings see the pending row through the normal
  store path;
- account/current-pubkey changes cannot change a pinned signer identity;
- signer absence survives restart as `AwaitingSigner` and resumes after attach;
- an invalid or mismatched signer response cannot promote the row;
- pre-signature cancellation restores a relay-observed displaced winner but
  never an obsolete unpublished local predecessor;
- kinds `0`, `3`, `10000...19999`, and same-`d` `30000...39999` retire older
  obligations atomically, destroy safely-unsent bodies and receipts, and
  recover only the newer open intent after restart;
- different `d` coordinates remain independent; a started attempt retains
  only bounded `Superseded` safety evidence and never preserves retry work;
- an exact-base guarded replacement is accepted, while a concurrent winner
  produces one typed terminal receipt-only refusal with no accepted intent,
  journal row, pending event, or downstream work;
- all relays rejecting a signed event leaves the signed row intact;
- transport reconnect cannot duplicate durable buffering ownership;
- at `max_relays = 1`, an ordinary public route-discovery query plus a durable
  write to the same relay progresses through exact single publish, ACK, and
  public-query restoration;
- a protected read emits only read admission and cannot claim the write's
  same-relay time-sharing authority;
- restart preserves attempt ordinal and next eligibility;
- at-most-once ambiguity never emits a second send;
- exact policy, signer, and correlated relay AUTH denials commit before emit
  and replay with the same source/reason after a real Redb reconstruction;
- `AuthRequired`, policy `Error`/`Unavailable`, and unrelated subscription
  `CLOSED` frames never create terminal write facts;
- a stale denial revision is refused even when the current terminal denial has
  equal source/reason, while a committed same-revision retry is idempotent;
- same-URL sessions for another identity and other lanes on the same receipt
  continue independently after one lane is AUTH-denied;
- the real-websocket BDD facade proof observes the first unauthenticated EVENT
  at the relay's raw socket and observes no further EVENT after restart.
