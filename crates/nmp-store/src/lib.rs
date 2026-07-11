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
//! Two architecture-review corrections load-bear on the above: (1)
//! [`IntentId`] is allocated by the STORE from a durable high-water mark
//! bumped inside `accept_write`'s own transaction — never caller-supplied
//! (see its doc for the reuse hazard this closes); (2) receipt identity/
//! state is retained under `OUTBOX_RECEIPTS`, independently of
//! `OUTBOX_INTENTS`'s open-work row, so [`EventStore::reattach_receipt`]
//! keeps answering for a terminal receipt after its open-work row is gone
//! (see [`ReceiptState`]'s doc).
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

/// Stable identifier for a durable write intent, ALLOCATED BY THE STORE
/// ITSELF from a durable, monotonically-advancing high-water mark
/// (`OUTBOX_META` for `RedbStore`) bumped inside the SAME `accept_write`
/// transaction that journals the intent — never inferred from the
/// currently-open set.
///
/// This is a load-bearing correction (architecture review, post-initial-
/// build): an earlier revision of this door took a CALLER-assigned
/// `IntentId` and left allocation to `nmp-engine`. That is unsound the
/// moment R8-style terminal cleanup exists: `OUTBOX_INTENTS` rows are
/// deleted once an intent's open work concludes (`compensate_write` today;
/// a future all-lanes-terminal path later), so a caller-side allocator that
/// infers "next free" from the currently-*open* recovered set will
/// eventually reissue an id that a terminated intent already used —
/// colliding with that intent's still-*retained* [`RecoveredReceipt`] (see
/// [`EventStore::reattach_receipt`]) or any retained per-relay attempt
/// evidence. Issue #3's "ids remain stable and unique across restart"
/// means unique for the store's ENTIRE lifetime, not merely among what
/// recovery currently sees open — so allocation must be a fact the store
/// itself owns and persists, never a value trusted in from outside.
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

/// A durable-persistence failure at the acceptance boundary
/// (`docs/design/durable-write-signing-and-retry.md` §1: "If that
/// transaction fails, the caller receives an acceptance error and no
/// pending row becomes visible" — architecture review correction).
/// Realistic runtime failures (disk full, I/O error) at `accept_write`/
/// `accept_ephemeral`/`promote_signed`/`compensate_write` must never panic
/// the embedding app — unlike the rest of this crate's `redb` usage, which
/// stays `.expect()`-on-invariant-violation per this crate's own module
/// doc (a healthy embedded DB file failing to open a table it created
/// itself is a bug; a write failing because the disk is full is not).
/// `MemoryStore` implements the same fallible signature for backend
/// uniformity but never actually returns `Err` (it does no I/O).
#[derive(Debug)]
pub struct PersistenceError(pub String);

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "durable-store persistence failure: {}", self.0)
    }
}

impl std::error::Error for PersistenceError {}

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
/// all — it keeps today's direct-publish path with no journal row and no
/// pending store row, but it is NOT receipt-less (VISION-ratified
/// correction): [`EventStore::accept_ephemeral`] still persists a
/// reattachable [`RecoveredReceipt`] with `intent_id: None`, exactly like
/// any durable-write receipt, just with no backing `OUTBOX_INTENTS` row.
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
///
/// NOTE: neither an `IntentId` nor a receipt id is a field here — the store
/// allocates BOTH, from durable high-water marks bumped inside this same
/// transaction, and hands both back on every journaled [`AcceptOutcome`]
/// variant. See [`IntentId`]'s doc for why a caller-supplied id of either
/// kind is unsound: issue #3's "receipt ids remain stable and unique
/// across restart" carries the IDENTICAL reuse hazard the moment receipts
/// are durably retained across restart (architecture review correction) —
/// an engine-side counter that resets on restart could hand out a receipt
/// id colliding with a retained `OUTBOX_RECEIPTS` row, making
/// `reattach_receipt` ambiguous.
pub struct AcceptWrite {
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
/// insert"), including `Kind5Processed`: a locally-composed kind:5 draft
/// runs the SAME author-verified tombstone-write processing `insert` runs
/// for a relay-observed kind:5, immediately, in this same transaction
/// (architecture review correction — issue #2's "no app optimistic
/// mirror" promise extends to local deletions too). Its tombstone claims
/// are PROVISIONAL while the intent is still pending — reversible by
/// `compensate_write`, committed to permanent by `promote_signed` — see
/// `Kind5StashRecord`'s doc (redb backend) / `Kind5Stash`'s doc (memory
/// backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// Brand-new pending row, no address competition. `intent_id`/
    /// `receipt_id` are the store-allocated ids (see [`IntentId`]'s doc) —
    /// the ONLY place a caller learns either.
    Inserted {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
    },
    /// This exact event id was already held (see `Provenance::local_origin`'s
    /// doc — an edge case, not the relay-echo hand-off, which goes through
    /// ordinary `insert`/dedup instead). Still allocates and journals a
    /// fresh `intent_id`/`receipt_id` — this call is still a distinct
    /// accepted intent.
    Duplicate {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
    },
    /// The pending row won a replaceable/addressable address, evicting
    /// `replaced` — durably stashed by the caller into `OUTBOX_DISPLACED`
    /// in the SAME transaction, so pre-signature compensation
    /// (`compensate_write`) can restore it (retraction doc §4.2).
    Superseded {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
        replaced: Box<StoredEvent>,
    },
    /// This intent lost its address race to an existing, newer winner.
    /// The intent is still journaled (still gets signed and delivered —
    /// only `Refused` below skips the journal) but produces no pending row.
    Stale {
        intent_id: IntentId,
        receipt_id: u64,
    },
    /// A locally-composed kind:5 (NIP-09) deletion, stored like any other
    /// pending row through this door AND, in the SAME transaction, running
    /// the identical author-verified tombstone-write processing `insert`
    /// runs for a relay-observed kind:5 (architecture review correction:
    /// issue #2's "no app optimistic mirror" promise extends to
    /// locally-composed deletions too — the targets disappear from the
    /// LOCAL replica immediately, before any relay round-trip, not only
    /// once the relay echoes this deletion back through `insert`'s
    /// dedup-by-id branch). `deleted` holds every currently-held target
    /// this deletion actually removed, mirroring `InsertOutcome::
    /// Kind5Processed`. Returned in place of `Inserted` only for this one
    /// case — kind:5 has no replaceable/addressable address, so it can
    /// never reach `Superseded`/`Stale` by construction.
    Kind5Processed {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
        deleted: Vec<StoredEvent>,
    },
    /// Refused at the door — the same tombstone/expiry refusal `insert`
    /// runs. Terminal typed failure to the caller (R3): NOTHING is
    /// journaled — no intent row, no pending row, no receipt residue, and
    /// (correspondingly) no `IntentId`/receipt id is ever allocated for a
    /// refused call, so refusal can never "burn" either.
    Refused(RefuseReason),
}

impl AcceptOutcome {
    /// The `IntentId` this call journaled, if any — `None` only for
    /// `Refused` (R3: nothing was ever journaled, and no id was ever
    /// allocated for a refused call).
    pub fn journaled_intent_id(&self) -> Option<IntentId> {
        match self {
            AcceptOutcome::Inserted { intent_id, .. }
            | AcceptOutcome::Duplicate { intent_id, .. }
            | AcceptOutcome::Superseded { intent_id, .. }
            | AcceptOutcome::Stale { intent_id, .. }
            | AcceptOutcome::Kind5Processed { intent_id, .. } => Some(*intent_id),
            AcceptOutcome::Refused(_) => None,
        }
    }

    /// The store-allocated receipt id this call journaled, if any — `None`
    /// only for `Refused` (architecture review correction: receipt ids are
    /// store-allocated the same way `IntentId` is, and a refusal burns
    /// neither).
    pub fn journaled_receipt_id(&self) -> Option<u64> {
        match self {
            AcceptOutcome::Inserted { receipt_id, .. }
            | AcceptOutcome::Duplicate { receipt_id, .. }
            | AcceptOutcome::Superseded { receipt_id, .. }
            | AcceptOutcome::Stale { receipt_id, .. }
            | AcceptOutcome::Kind5Processed { receipt_id, .. } => Some(*receipt_id),
            AcceptOutcome::Refused(_) => None,
        }
    }
}

/// The result of an [`EventStore::promote_signed`] call — keyed by
/// `IntentId`, not the frozen event's id (architecture review correction: a
/// `Duplicate`/`Stale` intent never won a live row at its own id at all,
/// and a once-live row can since have been superseded, kind:5-deleted, or
/// expired). Three cases, all reachable: the intent's row is still live at
/// its own id (sentinel swapped for `sig` in place — same id, same
/// EVENTS/ADDR_INDEX/BY_AUTHOR/BY_KIND entries, zero churn); the intent's
/// frozen bytes are sitting in some OTHER intent's `OUTBOX_DISPLACED` stash
/// (chained local supersession before this intent could sign — the real
/// signature is synced into that stash entry too, so a future restore of
/// it never resurrects a stale sentinel copy of an intent that actually
/// signed); or neither (the row is gone for some unrelated reason — relay
/// supersession, kind:5 deletion, NIP-40 expiry — and the signed bytes are
/// synthesized from the journal's own copy so the engine can still publish
/// them even though this intent wins no local address). Either way,
/// `SigState`/`IntentSigState` flip to `Signed`, the durable
/// `OUTBOX_DISPLACED` stash for THIS intent (if any) is deleted in the same
/// transaction (R6), and — if this was a pending kind:5 draft — its
/// provisional tombstone claims become authoritative (see
/// `Kind5StashRecord`'s doc). Boxed for the same reason
/// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
/// small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    Promoted {
        row: Box<StoredEvent>,
    },
    /// This `IntentId` names no still-open intent: already promoted (a
    /// repeat promotion is a no-op, not a re-signature — codex-nova
    /// finding), already compensated, or never accepted through
    /// `accept_write`.
    NotFound,
}

/// The result of an [`EventStore::compensate_write`] call — keyed by
/// `IntentId`, same three-case dispatch [`PromoteOutcome`] documents (live
/// row / displaced-in-another-intent's-stash / neither). If live, the row
/// is removed (`remove(id, Rejected)` — no tombstone, the row was never
/// validly signed); if sitting in another intent's stash, that stash entry
/// is invalidated so the intent that displaced it can never later
/// resurrect a cancelled predecessor via ITS OWN compensation. Either way,
/// THIS intent's own displaced predecessor (if any) is restored through
/// the same one door and returned here (`None` if it displaced nothing, or
/// the re-offered predecessor came back `Stale` — retraction doc §3.4).
/// If this was a pending kind:5 draft, its provisional tombstone claims and
/// every target it removed are atomically reversed (see
/// `Kind5StashRecord`'s doc) — cancelling a delete brings the content back,
/// not merely closes the journal. The intent's `OUTBOX_INTENTS`/
/// `OUTBOX_DISPLACED`/kind:5-stash rows were all deleted in the same
/// transaction. Boxed for the same reason `InsertOutcome::Superseded` is:
/// keeps the common `NotFound` variant small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensateOutcome {
    Compensated {
        restored: Option<Box<StoredEvent>>,
    },
    /// This `IntentId` names no still-open intent: already promoted
    /// (compensation is pre-signature only, retraction doc §4.2's
    /// "Promotion correction"), already compensated, or never accepted
    /// through `accept_write`.
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

/// A durably-retained receipt's coarse status — the STORE-OBSERVABLE
/// subset of the full receipt stream (`nmp-engine`'s `WriteStatus` owns
/// the complete enum, including per-relay `Routed`/`Sent`/`Acked`/
/// `Rejected`/`GaveUp`/`Failed`; this crate only knows what its OWN four
/// doors did to a receipt). Retained under `OUTBOX_RECEIPTS` — separately
/// from `OUTBOX_INTENTS`'s open-work row — precisely so a receipt stays
/// reattachable via [`EventStore::reattach_receipt`] after the open-work
/// row is gone (architecture review correction: R8-style terminal cleanup
/// of `OUTBOX_INTENTS` must never also delete receipt identity/state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptState {
    /// `accept_write` (durable/`AtMostOnce`) or `accept_ephemeral`
    /// (`Ephemeral`) ran; nothing else has happened to this receipt yet.
    Accepted,
    /// `promote_signed` ran; the row carries a real signature. (Per-relay
    /// delivery evidence beyond this point is a later unit's job — the
    /// durable attempt table this frame only creates the schema for.)
    Signed,
    /// `compensate_write` ran; the pending row was retracted pre-signature
    /// (retraction doc §4.2). Terminal — a compensated intent never
    /// promotes.
    Compensated,
    /// An `Ephemeral` receipt (see [`EventStore::accept_ephemeral`]) that
    /// was still `Accepted` when the store reopened after a restart.
    /// `Ephemeral` writes are NEVER retried after process loss (R4), and
    /// this unit builds no dispatch/ack tracking that could have advanced
    /// it past `Accepted` before the crash — so an `Accepted` ephemeral
    /// receipt surviving to the NEXT `RedbStore::open()` can only mean the
    /// process died before any further transition was ever recorded.
    /// `RedbStore::open()` reconciles every such row to `Abandoned` in the
    /// same boot pass, mirroring how NIP-40 catches up expired-while-dead
    /// events at boot (retraction-and-negative-deltas.md §3.3). Terminal.
    Abandoned,
}

/// A durably-retained receipt record, independent of whether the intent's
/// open-work row (`OUTBOX_INTENTS`/[`RecoveredIntent`]) still exists —
/// see [`ReceiptState`]'s doc for why this separation exists. This unit
/// builds no pruning policy for these rows (mirrors how the retry-owner
/// follow-up, not this frame, owns `OUTBOX_ATTEMPTS` retention policy);
/// they simply accumulate until a later unit defines a retention/GC rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredReceipt {
    pub receipt_id: u64,
    /// `Some` for a durable/`AtMostOnce` receipt backed by a real (open or
    /// since-closed) `accept_write` intent. `None` for an `Ephemeral`
    /// receipt (`accept_ephemeral`): a receipt-ONLY record — VISION's
    /// "durable OR explicitly non-durable write is still observed through
    /// a reattachable receipt" promise, without ever entering the
    /// delivery-retry journal or gaining a query-visible pending row.
    pub intent_id: Option<IntentId>,
    pub frozen_id: EventId,
    pub expected_pubkey: PublicKey,
    pub state: ReceiptState,
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
    /// `Accepted`. `Refused` writes nothing at all (R3). A locally-composed
    /// kind:5 draft additionally runs the identical author-verified
    /// tombstone-write processing `insert` runs for a relay-observed
    /// kind:5, in the SAME transaction (architecture review correction:
    /// issue #2's immediate-delete promise extends to local compositions,
    /// not only the relay echo) — see `AcceptOutcome::Kind5Processed`.
    ///
    /// Fallible (architecture review correction,
    /// `docs/design/durable-write-signing-and-retry.md` §1: "if that
    /// transaction fails, the caller receives an acceptance error and no
    /// pending row becomes visible"): a realistic persistence failure
    /// (disk full, I/O error) returns `Err` rather than panicking the
    /// embedding app — unlike this crate's other, pre-existing doors,
    /// which remain `.expect()`-on-invariant-violation by design.
    /// `MemoryStore` never actually returns `Err` (no I/O).
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError>;

    /// Swap the sentinel signature on `intent_id`'s frozen body for the
    /// real `sig` and flip its `SigState`/`IntentSigState` to `Signed`, in
    /// the SAME transaction that durably drops the intent's own
    /// `OUTBOX_DISPLACED` stash (R6) and updates its retained receipt.
    /// Keyed by `IntentId`, NOT the frozen event's id (architecture review
    /// correction — load-bearing): the intent's `OUTBOX_INTENTS.frozen_json`
    /// is the durable source of truth for its body regardless of whether a
    /// live `EVENTS` row currently exists for it. Three cases, uniformly:
    /// (a) a live row still carries `local.intent_id == intent_id` — mutate
    /// it in place (same id — a NIP-01 id never depends on `sig` — so this
    /// is a value update, not a remove/re-add); (b) no live row, but this
    /// intent's exact frozen bytes are sitting in some OTHER intent's
    /// `OUTBOX_DISPLACED` stash (it was superseded by a later local edit
    /// before it could sign) — sync the real signature into that stash
    /// entry too, so a future restore of it never resurrects a stale
    /// sentinel copy; (c) neither (the intent was `Stale`/`Duplicate` at
    /// acceptance, or its row was since superseded by a RELAY-observed
    /// event, kind:5-deleted, or NIP-40-expired) — mutate only the durable
    /// `OUTBOX_INTENTS`/`OUTBOX_RECEIPTS` journal copies; the resulting
    /// signed bytes are still returned so the engine can publish them even
    /// though this intent does not (or no longer) wins any local address.
    /// The caller must have already validated `sig` against the frozen
    /// body/pubkey/id (`nostr::Event::verify`) — this door does not
    /// re-verify (signature verification is explicitly out of scope for
    /// this crate). Fallible for the same reason `accept_write` is.
    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        sig: Signature,
    ) -> Result<PromoteOutcome, PersistenceError>;

    /// Pre-signature compensation only (retraction doc §4.2's "Promotion
    /// correction": once `promote_signed` has run, relay ACK/reject/timeout
    /// is receipt-only and NEVER reaches this door — a `Signed` intent
    /// answers `NotFound` here). Keyed by `IntentId` (same architecture
    /// review correction as `promote_signed`, same three cases): (a) a live
    /// row still carries `local.intent_id == intent_id` — `remove(id,
    /// Rejected)` (no tombstone), then re-`insert` the intent's
    /// durably-stashed `displaced` predecessor (if any) through the same
    /// one door — it wins its address back by ordinary supersession, never
    /// an un-supersede operation; (b) no live row, but this intent's exact
    /// frozen bytes are sitting in some OTHER intent's `OUTBOX_DISPLACED`
    /// stash — that stash entry is invalidated (removed) for good: this
    /// intent is being permanently rejected, so the intent that displaced
    /// it must never later resurrect it via ITS OWN compensation; (c)
    /// neither — nothing to remove or restore in `EVENTS`. In every case,
    /// this intent's own `OUTBOX_INTENTS`/`OUTBOX_DISPLACED` rows are
    /// deleted and its retained receipt updated to `Compensated`. Fallible
    /// for the same reason `accept_write` is.
    fn compensate_write(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CompensateOutcome, PersistenceError>;

    /// Read every still-open intent back out of the durable journal on
    /// boot (issue #3 §2.3). Read-only: the pending rows themselves are
    /// already live in the store (committed at `accept_write` time) — this
    /// returns only the journal metadata `nmp-engine` needs to rebuild its
    /// in-memory write-outbox bookkeeping. `MemoryStore` always returns
    /// empty (Fable checkpoint Q4: crash-safety is a `RedbStore`-only
    /// backend property, not a contract `EventStore` itself promises).
    fn recover_outbox(&self) -> Vec<RecoveredIntent>;

    /// Look up `receipt_id`'s durably-RETAINED record — independent of
    /// whether its intent's `OUTBOX_INTENTS` open-work row still exists
    /// (architecture review correction: separates "recoverable open work"
    /// from "receipt identity/state", so a terminal receipt stays
    /// reattachable — issue #3's "receipts remain... reattachable" —
    /// rather than disappearing the moment its open-work row is cleaned
    /// up). Unlike `recover_outbox`, this is an ordinary retained-data
    /// lookup, not a boot-only replay: `MemoryStore` answers it faithfully
    /// for the life of the process (no Q4 "always empty" carve-out here —
    /// that carve-out is specifically about surviving a REAL crash, which
    /// this door never claims to do for a volatile backend).
    fn reattach_receipt(&self, receipt_id: u64) -> Option<RecoveredReceipt>;

    /// Persist a receipt-ONLY record for an `Ephemeral` write (VISION-
    /// ratified contract clarification, team-lead correction, issue #3):
    /// `Ephemeral` never enters the durable delivery-retry journal (no
    /// `OUTBOX_INTENTS`/`OUTBOX_ATTEMPTS` row — R4 stays correct, it is
    /// never retried after process loss) and never gains a query-visible
    /// pending row (no `EVENTS`/`accept_write` call at all) — but a
    /// durable OR explicitly non-durable write must still be observable
    /// through a reattachable receipt, so THIS door writes just the
    /// `OUTBOX_RECEIPTS` row: `RecoveredReceipt::intent_id` is `None`
    /// (nothing backs it — no intent, no journal, no pending event row),
    /// state starts `Accepted`. See [`ReceiptState::Abandoned`] for what
    /// happens to it if the process dies before any further transition.
    ///
    /// Returns the store-allocated receipt id — the same durable
    /// high-water-mark `accept_write` allocates from (architecture review
    /// correction: a caller-side receipt-id counter that resets on
    /// restart has the identical reuse hazard `IntentId` had, now that
    /// receipts are durably retained across restart). Fallible for the
    /// same reason `accept_write` is.
    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError>;
}
