//! `nmp-store` — `EventStore` trait + `MemoryStore` + `RedbStore`: the one
//! mutating door (VISION §4 "the store", bug-class ledger #1), extended in
//! M3 step A1 with persistence, provenance merge, and coverage watermarks
//! (VISION §7 ledger #7 / #5).
//!
//! Insert runs **dedup-by-id first**, THEN replaceable/addressable
//! supersession (M1 plan §2.2): winner = newest `created_at`, tie-break
//! lexicographically-smallest id. `query` reuses `nostr::Filter::match_event`
//! — no hand-rolled event matching. A duplicate-id insert now MERGES relay
//! provenance into the stored row (ledger #5) instead of being a no-op.
//!
//! Coverage (`record_coverage`/`get_coverage`) implements
//! `docs/consults/2026-07-11-fable-coverage-attribution.md` at the store
//! layer — see [`coverage`] for the full ruling recap. Claim-based bounded
//! GC (`gc`) evicts only regular (non-addressed) events matched by no live
//! claim, lowering any coverage row it invalidates in the same step.
//!
//! Retraction (`docs/design/retraction-and-negative-deltas.md`, issue #28):
//! kind:5 (NIP-09) deletion runs inside `insert` and writes PERMANENT
//! tombstones (§7 owner decision — never GC-claimed) so a later redelivery
//! of a deleted event is `Refused(Tombstoned)`; NIP-40 `expiration` is
//! tracked in a persistent index so `expire_due`/`next_expiration` are
//! index-backed, not O(stored rows).
//!
//! Durable write-outbox (`docs/design/crashsafe-accepted-2-3-plan.md`,
//! issues #2/#3, Fable checkpoint verdict Q2): this crate is now the event
//! **and** durable-outbox store — one atomic `redb::Database` boundary. A
//! locally-authored write intent enters through [`EventStore::accept_write`]
//! (the same dedup/tombstone/supersession rules `insert` runs, stamping
//! local provenance + [`SigState::Pending`] instead of a `RelayObserved`),
//! committing the pending row AND the durable intent/displaced-stash journal
//! in ONE transaction. [`EventStore::promote_signed`] swaps the real
//! signature in place (zero id churn — a NIP-01 id never depends on `sig`)
//! and durably drops the displaced stash. [`EventStore::compensate_write`]
//! undoes a pre-signature-terminated intent: `remove(id, Rejected)` (no
//! tombstone — the row was never validly signed) plus a compensating
//! re-`insert` of whatever it displaced, through the same one door.
//! [`EventStore::recover_outbox`] replays every still-open intent after a
//! restart. Every policy decision (retry ownership, deadline scheduling,
//! signer orchestration) stays in `nmp-engine`; the store exposes only these
//! typed doors — never raw table/transaction access.
//!
//! Explicitly out of scope for M3 step A1 (owned by later steps): signature
//! verification (the `nostr::Event::verify` call an accepted signer result
//! must pass happens in `nmp-engine`, before `promote_signed` is ever
//! called), the engine's send-time attribution snapshots (this crate only
//! stores whatever interval it is told to record).

mod address_key;
mod coverage;
mod memory_store;
mod redb_store;

pub use coverage::{coverage_key, ClaimSet, CoverageInterval, CoverageKey, GcReport};
pub use memory_store::MemoryStore;
pub use redb_store::RedbStore;

use std::collections::BTreeMap;

use nmp_grammar::ConcreteFilter;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, Filter, PublicKey, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize};

/// Stable identifier for a durable write intent, assigned by the caller
/// (`nmp-engine`) and persisted in `OUTBOX_INTENTS`/carried on the pending
/// row's [`LocalOrigin`] — unlike an in-memory receipt counter, it survives
/// restart. The store never allocates one, and never infers a "next free"
/// value from the currently-open set: a caller (U3/U4) MUST allocate from a
/// durable, monotonically-advancing high-water mark that is never reset by
/// recovery, so a value is never reused across the store's ENTIRE lifetime
/// — not just while an intent using it is still open. Seeding an allocator
/// only past the max *currently-open* recovered id is UNSOUND: R8 deletes
/// `OUTBOX_INTENTS` rows once an intent terminates, so an id freed that way
/// looks "never used" to a naive open-set scan and can collide with a
/// retained `OUTBOX_ATTEMPTS` row from the terminated intent that reused
/// it — issue #3's "receipt ids remain stable and unique across restart"
/// requires uniqueness for the store's whole lifetime, not merely among
/// what recovery currently sees open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IntentId(pub u64);

/// Signature state of a locally-authored row, as data on the row itself
/// (`docs/design/retraction-and-negative-deltas.md` §4.1 — "not a second
/// query path or committed/pending authority split"). Exposed on
/// [`LocalOrigin`] so the app surface can always tell a sentinel-sig
/// pending row from a really-signed one (Fable checkpoint Q1 condition a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SigState {
    /// The row's `sig` is [`sentinel_signature`] — not yet signed.
    Pending,
    /// The row carries a real, caller-verified signature.
    Signed,
}

/// A locally-authored row's provenance (issue #2's "`Local` origin; a row
/// *field*, exactly ledger #5's shape"). Set iff this row entered through
/// [`EventStore::accept_write`] rather than [`EventStore::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalOrigin {
    pub intent_id: IntentId,
    pub sig_state: SigState,
    pub accepted_at: Timestamp,
}

/// Per-relay provenance for one stored event: which relays have delivered
/// this exact event id, and the latest wall-clock time each one did so
/// (ledger #5). A first-class field of the stored row, not a sidecar.
/// `local` is `Some` iff the row was locally authored (issue #2) — it is
/// preserved (never cleared) across a later relay echo merging into `seen`:
/// the app's "sending…" chip resolves off `seen.is_empty()`, not off
/// `local`'s presence (retraction doc §4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    pub seen: BTreeMap<RelayUrl, Timestamp>,
    pub local: Option<LocalOrigin>,
}

impl Provenance {
    /// A fresh `Provenance` recording exactly one observation.
    pub(crate) fn first_observation(from: RelayObserved) -> Self {
        let mut seen = BTreeMap::new();
        seen.insert(from.relay, from.at);
        Self { seen, local: None }
    }

    /// A fresh `Provenance` for a row entering through `accept_write`: no
    /// relay has observed it yet, but it carries local provenance.
    pub(crate) fn local_origin(local: LocalOrigin) -> Self {
        Self {
            seen: BTreeMap::new(),
            local: Some(local),
        }
    }

    /// Merge one more observation in. Returns `true` iff this observation
    /// changed the map: a relay not seen before, or a strictly later
    /// timestamp for a relay already seen. A redelivery from a relay at an
    /// equal-or-earlier timestamp than what is already recorded changes
    /// nothing and returns `false` — no index churn on a no-op merge.
    /// Never touches `local` — a relay echo of an already-local row keeps
    /// its local provenance (retraction doc §4.1).
    pub(crate) fn merge_observation(&mut self, from: &RelayObserved) -> bool {
        match self.seen.get(&from.relay) {
            None => {
                self.seen.insert(from.relay.clone(), from.at);
                true
            }
            Some(existing) if *existing < from.at => {
                self.seen.insert(from.relay.clone(), from.at);
                true
            }
            Some(_) => false,
        }
    }
}

/// The sentinel signature every pending row's frozen body carries until
/// [`EventStore::promote_signed`] swaps in the real one (Fable checkpoint
/// Q1, APPROVED): a NIP-01 id is `hash([0,pubkey,created_at,kind,tags,
/// content])` — the signature is not an id input — so an all-zero 64-byte
/// value round-trips through `nostr::Event`/JSON/`Filter::match_event`
/// unverified (schnorr `Signature` parsing is length-checked only) and the
/// id is final before a real signature exists.
pub fn sentinel_signature() -> Signature {
    Signature::from_slice(&[0u8; 64])
        .expect("64 zero bytes is always a structurally valid (length-checked) schnorr signature")
}

/// A stored event plus its provenance. What `query` returns — every caller
/// gets provenance for free, never a bare `Event` (ledger #5's falsifier:
/// no `query` path returns an event without its provenance populated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub event: Event,
    pub provenance: Provenance,
}

/// Which relay delivered an event, and the engine's wall-clock time at
/// receipt — the `insert` door's second argument (M3 §3.1's `from:
/// RelayObserved`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayObserved {
    pub relay: RelayUrl,
    pub at: Timestamp,
}

impl RelayObserved {
    pub fn new(relay: RelayUrl, at: Timestamp) -> Self {
        Self { relay, at }
    }
}

/// The result of an [`EventStore::insert`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Brand-new event id, not part of any replaceable/addressable
    /// competition (or the first event at that address).
    Inserted,
    /// This exact event id is already present. `provenance_grew` is `true`
    /// iff the merge actually changed the provenance map (M1's no-op stub
    /// becomes a real merge in M3 — ledger #5).
    Duplicate { provenance_grew: bool },
    /// A replaceable/addressable winner changed. `replaced` is the evicted
    /// row itself, handed back whole: the store is holding it at the exact
    /// moment of eviction, and this is the only moment it can be returned
    /// (retraction-and-negative-deltas.md §1.1) — the resolver's dirty-seed
    /// and the optimistic-write rollback path both need to `match_event`
    /// and re-insert this row after the store has already dropped it.
    Superseded {
        /// The full row that was superseded (dropped from the store).
        /// Boxed so the common `Inserted`/`Duplicate`/`Stale` variants stay
        /// small — `Superseded` is the rare, eviction-only case.
        replaced: Box<StoredEvent>,
    },
    /// This event is older than the current winner for its
    /// replaceable/addressable address (or ties on `created_at` but does not
    /// win the lexicographic id tie-break). Rejected: dropped, never stored.
    Stale,
    /// Refused at the door: never stored, nothing to retract
    /// (retraction-and-negative-deltas.md §1.1/§2/§3).
    Refused(RefuseReason),
    /// A kind:5 (NIP-09) deletion event, stored normally like any other
    /// regular event — kind:5 is outside M1's replaceable/addressable set,
    /// so its own storage is always plain `Inserted` by construction, and
    /// this variant is returned in place of `Inserted` only for that one
    /// case. `deleted` holds every currently-held target this deletion
    /// actually removed (author-verified against this event's own pubkey),
    /// handed back whole — the only moment the door can return them,
    /// mirroring `Superseded { replaced }` (retraction-and-negative-
    /// deltas.md §2).
    Kind5Processed { deleted: Vec<StoredEvent> },
}

/// Why an [`EventStore::insert`] refused an event outright, before it ever
/// touched an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The event's NIP-40 `expiration` tag is already in the past at the
    /// moment of insert (checked against the `RelayObserved` clock the
    /// caller passed in). Wired in this unit.
    AlreadyExpired,
    /// The event's id (or, for an addressable/replaceable target, its
    /// address) was tombstoned by an earlier verified kind:5 deletion from
    /// the same author (retraction-and-negative-deltas.md §2, §7:
    /// tombstone retention is PERMANENT — never GC-claimed).
    Tombstoned,
}

/// Why an [`EventStore::remove`] call is removing a row. Exists so
/// diagnostics can count retractions per cause, and so `remove` reads as
/// self-documentingly *not* a general delete API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetractReason {
    /// An optimistic local write was rejected (or its whole intent failed)
    /// before ever being accepted.
    Rejected,
    /// Removed by a verified kind:5 deletion from the event's own author.
    Deleted,
    /// Removed because its NIP-40 `expiration` deadline passed.
    Expired,
}

/// Durability class of a write intent, as store-owned persisted data — the
/// store never interprets it (retry/backoff policy stays in `nmp-engine`,
/// crashsafe-accepted-2-3-plan.md §7 Q2's boundary constraint), it only
/// journals and returns it verbatim. `Ephemeral` is deliberately absent:
/// per the plan's R4, an `Ephemeral` write never reaches `accept_write` at
/// all — it keeps today's direct-publish path with no journal row, no
/// pending store row, no receipt (ledger #9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteDurability {
    Durable,
    AtMostOnce,
}

/// Journal-level signature state of an `OUTBOX_INTENTS` row (Fable
/// checkpoint R1) — a FINER granularity than the row-level [`SigState`]
/// the app sees: `AwaitingSigner` and `Pending` both project as
/// `SigState::Pending` to the app (both are "not yet signed"), but the
/// engine needs the extra distinction on restart to know whether a signer
/// attach should re-trigger `RequestSign` (`AwaitingSigner`) or whether a
/// sign request was already in flight and its response is simply lost
/// (`Pending` — safe to re-request; double-signing after a crash is
/// harmless, same id either valid signature promotes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentSigState {
    /// No signer for `expected_pubkey` was attached at acceptance.
    AwaitingSigner,
    /// A signer is (or was) in flight; the row's `sig` is still
    /// [`sentinel_signature`].
    Pending,
    /// [`EventStore::promote_signed`] has run; the row carries a real
    /// signature.
    Signed,
}

/// The full journal payload for one locally-accepted write intent (Fable
/// checkpoint R7): everything #3's "one crash-atomic commit" enumerates,
/// gathered into one struct so `accept_write` can commit it and the pending
/// row in a single `redb::WriteTransaction` — atomicity is structural, not
/// a calling convention.
pub struct AcceptWrite {
    pub intent_id: IntentId,
    /// The engine's own `ReceiptId`, persisted so it can be reattached
    /// after restart (issue #3: "receipt ids remain stable and unique
    /// across restart").
    pub receipt_id: u64,
    /// The frozen, unsigned NIP-01 body: pubkey/created_at/kind/tags/
    /// content are final and `event.id` is already `EventId::new(..)` over
    /// exactly those fields (the signature is not an id input — Q1).
    /// `event.sig` must be [`sentinel_signature`] until
    /// [`EventStore::promote_signed`] swaps in the real one.
    pub frozen: Event,
    /// The pinned signing identity (#43 "pins the chosen identity at
    /// acceptance"). Ordinarily equal to `frozen.pubkey`; kept as an
    /// explicit field because it is a distinct journal fact (#2's "expected
    /// pubkey"), not merely derivable convenience.
    pub expected_pubkey: PublicKey,
    /// Opaque placeholder the store persists and returns verbatim — #47
    /// gives it real meaning; this frame only pins the persistence hook
    /// (Fable checkpoint Q5).
    pub signing_identity_ref: String,
    pub durability: WriteDurability,
    /// Opaque, engine-owned routing snapshot at acceptance — persisted and
    /// returned verbatim by `recover_outbox`. The store never interprets
    /// routing semantics; §5's append-only-revision ownership stays in
    /// `nmp-engine`.
    pub routing: String,
    /// The intent's sig state AT ACCEPTANCE — always `AwaitingSigner` or
    /// `Pending`, never `Signed` (a row only reaches `Signed` through
    /// `promote_signed`).
    pub sig_state: IntentSigState,
    pub accepted_at: Timestamp,
}

/// The result of an [`EventStore::accept_write`] call — mirrors
/// [`InsertOutcome`]'s shape (Fable checkpoint: "reuses the widened
/// `Superseded` shape so the resolver sorts it exactly like a relay
/// insert"), minus `Kind5Processed`: a locally-composed kind:5 draft is
/// stored like any other pending row through this door; its NIP-09
/// tombstone *write* side effects apply once it is relay-observed (the
/// ordinary `insert` path), not at local acceptance — out of this frame's
/// scope (the plan requires only that `accept_write` reuse the tombstone
/// *refusal* check `insert` runs, not `insert`'s deletion-processing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// Brand-new pending row, no address competition.
    Inserted { row: StoredEvent },
    /// This exact event id was already held (see `Provenance::local_origin`'s
    /// doc — an edge case, not the relay-echo hand-off, which goes through
    /// ordinary `insert`/dedup instead).
    Duplicate { row: StoredEvent },
    /// The pending row won a replaceable/addressable address, evicting
    /// `replaced` — durably stashed by the caller into `OUTBOX_DISPLACED`
    /// in the SAME transaction, so pre-signature compensation
    /// (`compensate_write`) can restore it (retraction doc §4.2).
    Superseded {
        row: StoredEvent,
        replaced: Box<StoredEvent>,
    },
    /// This intent lost its address race to an existing, newer winner.
    /// The intent is still journaled (still gets signed and delivered —
    /// only `Refused` below skips the journal) but produces no pending row.
    Stale,
    /// Refused at the door — the same tombstone/expiry refusal `insert`
    /// runs. Terminal typed failure to the caller (R3): NOTHING is
    /// journaled — no intent row, no pending row, no receipt residue.
    Refused(RefuseReason),
}

/// The result of an [`EventStore::promote_signed`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// The sentinel signature was swapped for `sig` in place (same id, same
    /// EVENTS/ADDR_INDEX/BY_AUTHOR/BY_KIND entries — zero churn) and
    /// `SigState` flipped to `Signed`. The durable `OUTBOX_DISPLACED` stash
    /// for this intent (if any) was deleted in the same transaction (R6).
    /// Boxed for the same reason `InsertOutcome::Superseded` is: keeps the
    /// common `NotFound` variant small.
    Promoted { row: Box<StoredEvent> },
    /// No local pending row with this id — already promoted, already
    /// compensated, or never accepted through `accept_write`.
    NotFound,
}

/// The result of an [`EventStore::compensate_write`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensateOutcome {
    /// The pending row was removed (`remove(id, Rejected)` — no tombstone,
    /// the row was never validly signed) and, if it had displaced a
    /// predecessor, that predecessor was re-inserted through the same one
    /// door and is returned here (`None` if it displaced nothing, or the
    /// re-offered predecessor came back `Stale` — retraction doc §3.4). The
    /// intent's `OUTBOX_INTENTS`/`OUTBOX_DISPLACED` rows were deleted in
    /// the same transaction. Boxed for the same reason
    /// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
    /// small.
    Compensated { restored: Option<Box<StoredEvent>> },
    /// No local pending row with this id — already promoted (compensation
    /// is pre-signature only, retraction doc §4.2's "Promotion
    /// correction"), already compensated, or never accepted through
    /// `accept_write`.
    NotFound,
}

/// One still-open intent replayed by [`EventStore::recover_outbox`] on
/// boot. The pending row itself is NOT re-inserted — it is already live in
/// the store (committed atomically at `accept_write` time) and query-visible
/// from the first post-boot subscription; this is only the journal metadata
/// `nmp-engine` needs to rebuild its in-memory `PendingWrite`/
/// `event_to_receipt` bookkeeping (plan §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredIntent {
    pub intent_id: IntentId,
    pub receipt_id: u64,
    pub frozen: Event,
    pub expected_pubkey: PublicKey,
    pub signing_identity_ref: String,
    pub durability: WriteDurability,
    pub routing: String,
    pub sig_state: IntentSigState,
    /// The predecessor this intent displaced, if any — still durable
    /// (`OUTBOX_DISPLACED` is deleted only by `promote_signed` or
    /// `compensate_write`, never by `recover_outbox`), so a post-restart
    /// cancellation can still restore it.
    pub displaced: Option<StoredEvent>,
    pub accepted_at: Timestamp,
}

/// The single mutating door onto the event store.
pub trait EventStore {
    /// Insert an event observed via `from`. An already-expired event (NIP-40,
    /// judged against `from.at`) is `Refused` before anything else runs —
    /// never stored, nothing to retract. Otherwise dedup-by-id FIRST — on a
    /// hit, merge `from` into the existing row's provenance and return
    /// `Duplicate{provenance_grew}` with NO index churn. Next, a tombstone
    /// check (retraction-and-negative-deltas.md §2): an id (or address, at
    /// or before its permanently-recorded deletion ceiling) tombstoned by an
    /// earlier verified kind:5 is `Refused(Tombstoned)`, never stored.
    /// Otherwise run replaceable/addressable supersession (unchanged M1
    /// semantics). A kind:5 event is stored like any other regular event
    /// and, in the same call, drops every currently-held target it names
    /// whose author matches its own (NIP-09 author-only, enforced
    /// structurally) — see `Kind5Processed`.
    fn insert(&mut self, event: Event, from: RelayObserved) -> InsertOutcome;

    /// Query current winners only (never a superseded/stale event), matched
    /// via `nostr::Filter::match_event`, each with its provenance attached.
    fn query(&self, filter: &Filter) -> Vec<StoredEvent>;

    /// Remove `id` from the store — clearing both the id index and, if `id`
    /// is the current replaceable/addressable winner for its address, the
    /// address index too — and hand back the removed row whole, or `None`
    /// if `id` was not held. Engine-facing only (kind:5 processing,
    /// optimistic-write rejection); never a general delete API.
    fn remove(&mut self, id: EventId, reason: RetractReason) -> Option<StoredEvent>;

    /// Drain every row whose NIP-40 `expiration` is `<= now`, removing each
    /// one (through the same [`EventStore::remove`] door) and returning the
    /// full rows. Index-backed (retraction-and-negative-deltas.md §3.1): a
    /// persistent `(expiry_ts -> {id})` index is maintained on every insert
    /// and every removal, so this drains in `O(log n + due)`, not a full
    /// scan.
    fn expire_due(&mut self, now: Timestamp) -> Vec<StoredEvent>;

    /// The earliest NIP-40 `expiration` deadline among currently stored
    /// rows, or `None` if nothing carries one. Index-backed: peeks the
    /// minimum of the same persistent expiration index `expire_due` drains.
    fn next_expiration(&self) -> Option<Timestamp>;

    /// Record that `relay` has proven `proven` for `filter`'s window-erased
    /// shape (ruling §1/§3). Merge-only: no public lowering path exists
    /// outside `gc`.
    fn record_coverage(
        &mut self,
        filter: &ConcreteFilter,
        relay: &RelayUrl,
        proven: CoverageInterval,
    );

    /// The proven interval for `key` at `relay`, or `None` if no row exists
    /// — "no row = not covered" (harvest rule, unchanged). `None` is
    /// authoritative-unknown, never treated as authoritative-empty.
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval>;

    /// Claim-based bounded GC (ruling §5): evicts every regular
    /// (non-replaceable, non-addressable) event matched by NO claim in
    /// `claims`. A claimed event, and every replaceable/addressable current
    /// winner, are ALWAYS retained — winners are never GC candidates at all,
    /// regardless of `claims`. When an evicted event falls inside a coverage
    /// row's proven interval and that row's retained shape matches it, the
    /// row is shrunk (or deleted, if the shrink empties it) in the same step
    /// — a watermark must never claim coverage of data no longer held.
    ///
    /// GC exclusion for open intents (Fable checkpoint R5): a row with
    /// local provenance still in `SigState::Pending` is NEVER a GC
    /// candidate, regardless of `claims` — structurally the same
    /// unconditional retention already given to replaceable/addressable
    /// winners, so an unsigned pending row can never be evicted before it
    /// ever signs. Once `promote_signed` flips it to `Signed`, it is an
    /// ordinary event again, GC-able like any other under `claims`.
    fn gc(&mut self, claims: &ClaimSet) -> GcReport;

    /// Accept a durably-owned local write intent (issues #2/#3): runs the
    /// SAME tombstone-refusal and replaceable/addressable supersession
    /// rules `insert` runs against `accept.frozen`, but stamps
    /// `Provenance::local_origin` instead of a `RelayObserved`, and commits
    /// the resulting row together with `accept`'s full journal payload
    /// (`OUTBOX_INTENTS` + `OUTBOX_DISPLACED`, if a predecessor was
    /// evicted) in ONE transaction (Fable checkpoint R7) — a crash mid-call
    /// leaves either nothing recoverable or a fully `recover_outbox`-able
    /// `Accepted`. `Refused` writes nothing at all (R3).
    fn accept_write(&mut self, accept: AcceptWrite) -> AcceptOutcome;

    /// Swap the sentinel signature on the local pending row `id` for the
    /// real `sig`, in place (same id — a NIP-01 id never depends on `sig`
    /// — so this is a value update, not a remove/re-add), and flip its
    /// `SigState` to `Signed`. In the SAME transaction: `OUTBOX_INTENTS`'s
    /// `sig_state` flips to `Signed`, and the intent's `OUTBOX_DISPLACED`
    /// stash (if any) is durably deleted (R6) — recovery after a promote
    /// must never see a stale displaced stash. The caller must have already
    /// validated `sig` against the frozen body/pubkey/id
    /// (`nostr::Event::verify`) — this door does not re-verify (signature
    /// verification is explicitly out of scope for this crate).
    fn promote_signed(&mut self, id: EventId, sig: Signature) -> PromoteOutcome;

    /// Pre-signature compensation only (retraction doc §4.2's "Promotion
    /// correction": once `promote_signed` has run, relay ACK/reject/timeout
    /// is receipt-only and NEVER reaches this door). In ONE transaction:
    /// `remove(id, Rejected)` (no tombstone), re-`insert` the intent's
    /// durably-stashed `displaced` predecessor (if any) through the same
    /// one door — it wins its address back by ordinary supersession, never
    /// an un-supersede operation — and delete the intent's
    /// `OUTBOX_INTENTS`/`OUTBOX_DISPLACED` rows.
    fn compensate_write(&mut self, id: EventId) -> CompensateOutcome;

    /// Read every still-open intent back out of the durable journal on
    /// boot (issue #3 §2.3). Read-only: the pending rows themselves are
    /// already live in the store (committed at `accept_write` time) — this
    /// returns only the journal metadata `nmp-engine` needs to rebuild its
    /// in-memory write-outbox bookkeeping. `MemoryStore` always returns
    /// empty (Fable checkpoint Q4: crash-safety is a `RedbStore`-only
    /// backend property, not a contract `EventStore` itself promises).
    fn recover_outbox(&self) -> Vec<RecoveredIntent>;
}
