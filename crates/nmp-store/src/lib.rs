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
//! restart. Exact resolved relay sets use a separate append-only route-
//! revision door which commits before any corresponding attempt. Every policy
//! decision (retry ownership, deadline scheduling, signer orchestration) stays
//! in `nmp-engine`; the store exposes only typed doors — never raw table/
//! transaction access.
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
mod binary_event;
mod coverage;
mod memory_store;
mod persistent_store_lifetime;
mod redb_store;

pub use coverage::{coverage_key, ClaimSet, CoverageInterval, CoverageKey, GcReport};
pub use memory_store::MemoryStore;
pub use persistent_store_lifetime::RedbStoreResetError;
pub use redb_store::RedbStore;

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ContextualAtom;
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
///
/// `owners` is a SET, not a single `IntentId` (architecture review
/// correction, team-lead decision on issue #2): an earlier revision
/// conflated "this row's canonical signature state" with "the one intent
/// that backs it," which broke the moment a byte-identical `Duplicate`
/// intent was accepted against an already-locally-owned row — cancelling
/// the FIRST intent would remove the row out from under a SECOND intent
/// still durably obligated to deliver it (its own `OUTBOX_INTENTS`/receipt
/// stayed open with no canonical row to promote or compensate). Every
/// accepted intent that currently backs this row's existence is a member;
/// coalescing duplicates into one owner was rejected because it would
/// silently drop a later intent's own receipt, violating "every accepted
/// write returns a receipt." `sig_state` stays canonical to the ROW, never
/// per-owner: ANY owner's [`EventStore::promote_signed`] call sets it, in
/// place, for every owner at once — there is exactly one signature on one
/// row, however many intents are backing it.
///
/// [`EventStore::compensate_write`] on one owner only removes THAT owner
/// from the set; the canonical row is only actually retracted once the set
/// is empty AND `sig_state` is still `Pending` AND no relay has
/// independently confirmed it (`Provenance::seen` empty) — an owner-less
/// row that is already `Signed`, or that a relay has confirmed on its own,
/// is left standing with an empty `owners` set rather than deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalOrigin {
    pub owners: BTreeSet<IntentId>,
    pub sig_state: SigState,
}

/// Per-relay provenance for one stored event: which relays have delivered
/// this exact event id, and the latest wall-clock time each one did so
/// (ledger #5). A first-class field of the stored row, not a sidecar.
/// `local` is `Some` iff this row has ever been locally accepted (issue
/// #2) — it is preserved (never cleared) across a later relay echo merging
/// into `seen`, AND across every owning intent eventually being
/// compensated away (`LocalOrigin::owners` can be empty while `local`
/// stays `Some`, e.g. once relay provenance alone sustains the row — see
/// [`LocalOrigin`]'s doc): the app's "sending…" chip resolves off
/// `seen.is_empty()`, not off `local`'s presence (retraction doc §4.1).
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
    Duplicate {
        provenance_grew: bool,
        /// Locally-accepted intent owners that this verified relay copy
        /// atomically advanced from Pending to Signed. The engine must route
        /// each matching obligation exactly once; an empty set is the common
        /// ordinary-dedup case.
        satisfied_intents: Vec<IntentId>,
    },
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
    /// A whole-value replacement was composed from `expected`, but the
    /// canonical winner at that exact replaceable/addressable coordinate
    /// was `actual` when the store's atomic acceptance transaction ran.
    /// Nothing was stored or journaled and no ids were allocated.
    ReplaceableBaseChanged {
        expected: Option<EventId>,
        actual: Option<EventId>,
    },
    /// A caller attached a replaceable-base precondition to an event kind
    /// that has no replaceable/addressable coordinate. Fail closed instead
    /// of silently accepting an unchecked write.
    ReplaceableBaseOnRegularEvent,
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
    /// Optional compare-and-swap guard for a whole-value replacement. The
    /// store derives the coordinate from `frozen` and compares its current
    /// canonical winner inside the same transaction that would accept the
    /// new row. `Some(None)` means the caller observed no local base;
    /// `None` means this is an ordinary, unconditional write.
    pub replaceable_base: Option<Option<EventId>>,
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
/// immediately, in the SAME transaction, stages a REVERSIBLE suppression
/// claim over every target it names — hiding whatever row currently lives
/// there from `query` WITHOUT moving or removing it (architecture review
/// correction — issue #2's "no app optimistic mirror" promise extends to
/// local deletions too). This replaced an earlier, withdrawn design that
/// physically moved a target row into a per-intent stash: codex-nova found
/// that made the target's OWN `promote_signed`/`compensate_write` blind to
/// it (a stashed row is invisible to anyone searching `EVENTS`/
/// `OUTBOX_DISPLACED`), and made an exact-`Duplicate` kind:5 intent's
/// promotion unsound (promoting it committed a real, permanent deletion
/// with no stash of its own to drop). The suppression-claim model fixes
/// both: rows never move, so every other door keeps working on exactly
/// the row it always did — a claim is pure, reversible metadata.
/// `compensate_write` drops a still-pending intent's claims outright (the
/// target reappears immediately — nothing to re-insert, it never left);
/// `promote_signed` drops them AND commits the deletion for real (the same
/// author-verified tombstone-write processing `insert` runs for a
/// relay-observed kind:5) — permanent from that point on
/// (retraction-and-negative-deltas.md §7).
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
    /// accepted intent, joining the existing row's owner set (issue #2's
    /// ownership-set model — see `LocalOrigin`'s doc) rather than being
    /// silently discarded. If the existing row (locally owned OR purely
    /// relay-observed — either way its `event.sig` is already real, not a
    /// sentinel) is ALREADY signed, this intent's OWN journal/receipt are
    /// journaled `Signed` from the start rather than `Pending` (codex-nova
    /// ruling): an offline co-owner signer must never strand a receipt
    /// behind an event that's already validly signed, and there is
    /// nothing left for this intent to sign.
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
    /// pending row through this door AND, in the SAME transaction, staging
    /// a provisional suppression claim over every target it names — the
    /// targets disappear from `query` immediately, before any relay
    /// round-trip (architecture review correction: issue #2's "no app
    /// optimistic mirror" promise extends to locally-composed deletions
    /// too), without being moved or removed. `hidden` holds every
    /// currently-visible row this claim just hid — both e-tag id targets
    /// and, unlike the deferred-to-promotion treatment an earlier
    /// revision gave them, a-tag address targets' current winners too
    /// (suppression is cheap and reversible either way, so there is no
    /// reason left to defer). Returned in place of `Inserted` only for
    /// this one case — kind:5 has no replaceable/addressable address, so
    /// it can never reach `Superseded`/`Stale` by construction.
    Kind5Processed {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
        hidden: Vec<StoredEvent>,
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
/// `Duplicate`/`Stale` intent with no shared row never won a live row at
/// its own id at all, and a once-live row can since have been superseded,
/// kind:5-deleted, or expired). Three cases, all reachable: `intent_id` is
/// a MEMBER of a live row's owner set (issue #2, team-lead decision —
/// ownership is a SET, so an exact `Duplicate` sharing an already-locally-
/// owned row is a CO-OWNER of it, not a row of its own) — sentinel swapped
/// for `sig` in place, same id, same EVENTS/ADDR_INDEX/BY_AUTHOR/BY_KIND/BY_TAG
/// entries, zero churn; `intent_id` is a member of some OTHER intent's
/// `OUTBOX_DISPLACED` stash entry's owner set (chained local supersession
/// before this intent could sign — the real signature is synced into that
/// stash entry too, so a future restore of it never resurrects a stale
/// sentinel copy of an intent that actually signed); or neither (the row
/// is gone for some unrelated reason — relay supersession, kind:5
/// deletion, NIP-40 expiry — and the signed bytes are synthesized from the
/// journal's own copy so the engine can still publish them even though
/// this intent wins no local address).
///
/// codex-nova ruling (issue #2's ownership-set model, tightened after
/// review): the FIRST owner to sign atomically transitions EVERY other
/// co-owner's own `OUTBOX_INTENTS`/`OUTBOX_RECEIPTS` row to `Signed`
/// against the SAME canonical bytes, in this SAME call — never lazily,
/// deferred until (or unless) each co-owner separately calls
/// `promote_signed` itself. An offline co-owner signer that never calls
/// back must not strand its receipt behind an event that is already
/// validly signed. `co_signed` names every OTHER intent this call just
/// advanced this way, so the caller can advance each of THEIR routing
/// obligations too, not only `intent_id`'s own. A co-owner's OWN later
/// call (e.g. its signer's delayed callback) now correctly answers
/// `NotFound` — its journal is already `Signed` by the time it calls, so
/// the existing per-intent guard catches it (see `NotFound`'s doc).
///
/// Either way, `SigState`/`IntentSigState` flip to `Signed`, the durable
/// `OUTBOX_DISPLACED` stash for `intent_id` AND every co-owner named in
/// `co_signed` is deleted in the same transaction (R6), and — if this was
/// a pending kind:5 draft — every owner's suppression claims become
/// authoritative permanent tombstones together. Boxed for the same reason
/// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
/// small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    Promoted {
        row: Box<StoredEvent>,
        /// Every OTHER co-owner `IntentId` this call ALSO atomically
        /// transitioned to `Signed` against the SAME canonical bytes (see
        /// this enum's own doc for why) — empty when `intent_id` is the
        /// row's only owner, which is the common case.
        co_signed: Vec<IntentId>,
    },
    /// This `IntentId` names no still-open intent, OR its OWN journal is
    /// ALREADY `Signed` — either because it promoted before (codex-nova's
    /// original repeat-promotion finding), or because some OTHER co-owner
    /// promoted first and this call's `co_signed` already advanced it
    /// (this intent's own delayed signer callback arriving after the
    /// fact). Also covers already compensated, or never accepted through
    /// `accept_write`.
    NotFound,
}

/// The result of an [`EventStore::compensate_write`] call — keyed by
/// `IntentId`, same three-case dispatch [`PromoteOutcome`] documents (live
/// row / displaced-in-another-intent's-stash / neither), same ownership-SET
/// model (issue #2, team-lead decision). If live, `intent_id` is removed
/// from the row's owner set; the row is only actually `remove(id,
/// Rejected)`-ed (no tombstone — the row was never validly signed) once
/// the set is EMPTY, `SigState` is still `Pending`, AND no relay has
/// independently confirmed it — an exact `Duplicate`'s still-open
/// obligation, an already-`Signed` state some OTHER co-owner committed, or
/// independent relay provenance, all survive THIS one intent's
/// cancellation (see `LocalOrigin`'s doc). If sitting in another intent's
/// stash, the SAME conditional removal applies to that stash entry's
/// owner set instead of dropping it outright. Either way, THIS intent's
/// own displaced predecessor (if any) is restored through the same one
/// door and returned here (`None` if it displaced nothing, or the
/// re-offered predecessor came back `Stale` — retraction doc §3.4).
/// If this was a pending kind:5 draft, this intent's OWN suppression
/// claims are dropped outright — every target it named reappears in
/// `query` immediately, with `revealed` listing the ones that ACTUALLY
/// became newly visible: a true visibility DELTA (architecture review
/// correction), computed from before/after suppression state and deduped
/// by event id, so a target still hidden by some OTHER intent's
/// overlapping claim, one already permanently removed by an intent that
/// promoted its own deletion of the same target, or one this claim's own
/// author/ceiling component never actually covered in the first place, is
/// correctly excluded. Nothing is ever re-inserted for `revealed`: a
/// suppressed row never left `EVENTS` in the first place — cancelling a
/// delete brings the content back, not merely closes the journal. The
/// intent's `OUTBOX_INTENTS`/`OUTBOX_DISPLACED`/suppression-claim rows
/// were all deleted in the same transaction. Boxed for the same reason
/// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
/// small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensateOutcome {
    Compensated {
        restored: Option<Box<StoredEvent>>,
        revealed: Vec<StoredEvent>,
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

/// Versioned, durable evidence for one publication attempt. The key is the
/// full `(intent, relay, ordinal)` tuple: a restart can never confuse a new
/// send with an older ambiguous send, and the exact signed bytes are retained
/// rather than reconstructed from mutable routing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredAttempt {
    pub version: u8,
    pub intent_id: IntentId,
    pub relay: RelayUrl,
    pub ordinal: u64,
    pub event: Event,
    pub outcome: AttemptOutcome,
}

/// Stable identity of one durable publication lane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LaneKey {
    pub intent_id: IntentId,
    pub relay: RelayUrl,
}

/// The current, versioned cursor for one `(intent, relay)` obligation.
/// History remains in the route/attempt/detail tables; this is the bounded
/// authoritative row recovery and scheduling read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredLane {
    pub version: u8,
    pub key: LaneKey,
    pub revision: u64,
    pub last_ordinal: u64,
    pub state: LaneState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaneState {
    WaitingConnection,
    WaitingAuth,
    Eligible {
        since: Timestamp,
    },
    InFlight {
        ordinal: u64,
        phase: InFlightPhase,
    },
    Transient {
        ordinal: u64,
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    },
    /// A v1 `Started` fact discovered during additive-schema bootstrap.
    /// The engine, not the store, decides how durability resolves it.
    LegacyInFlight {
        ordinal: u64,
    },
    Terminal {
        ordinal: u64,
        outcome: AttemptOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InFlightPhase {
    AwaitingHandoff,
    AwaitingAck { deadline: Timestamp },
}

/// Ordered deadline-index discriminator. Retry eligibility and ACK timeout
/// share one index but remain impossible to conflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeadlineKind {
    RetryEligible,
    AckTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneDeadline {
    pub at: Timestamp,
    pub key: LaneKey,
    pub lane_revision: u64,
    pub kind: DeadlineKind,
}

/// Transport handoff evidence, deliberately independent of nmp-transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoffEvidence {
    NotHandedOff,
    Written,
    Ambiguous,
}

/// Closed persistence vocabulary selected by the engine. The store never
/// maps transport outcomes into one of these causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransientCause {
    Interrupted,
    AckTimeout,
    ConnectionLost,
    RelayRateLimited,
    RelayError,
    AuthRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptHandoffDetail {
    pub at: Timestamp,
    pub result: HandoffEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptTransientDetail {
    pub eligible_at: Timestamp,
    pub cause: TransientCause,
    pub raw_reason: Option<String>,
}

/// Additive evidence beside a v1 attempt row. New rows are immutable
/// `Started` facts; upgrade reads also accept terminal rows written by the
/// pre-detail implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredAttemptDetails {
    pub version: u8,
    pub intent_id: IntentId,
    pub relay: RelayUrl,
    pub ordinal: u64,
    pub started_at: Option<Timestamp>,
    pub handoff: Option<AttemptHandoffDetail>,
    #[serde(default)]
    pub transient: Option<AttemptTransientDetail>,
    pub finished_at: Option<Timestamp>,
    pub terminal: Option<AttemptOutcome>,
}

pub(crate) fn attempt_is_live(
    attempt: &RecoveredAttempt,
    details: Option<&RecoveredAttemptDetails>,
) -> bool {
    if attempt.outcome != AttemptOutcome::Started {
        return false;
    }
    match details {
        Some(details) if details.terminal.is_some() || details.transient.is_some() => false,
        Some(details)
            if matches!(
                details.handoff,
                Some(AttemptHandoffDetail {
                    result: HandoffEvidence::NotHandedOff,
                    ..
                })
            ) =>
        {
            false
        }
        _ => true,
    }
}

/// Caller-selected post-handoff persistence state. This is a fact-writing
/// vocabulary, not a classification policy: the engine chooses the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostHandoffState {
    WaitingConnection,
    WaitingAuth,
    Eligible {
        since: Timestamp,
    },
    AwaitingAck {
        deadline: Timestamp,
    },
    Transient {
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    },
    Terminal {
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseIntentOutcome {
    Closed,
    AlreadyClosed,
}

/// One append-only snapshot of the exact relay set resolved for an intent.
/// It is committed before any corresponding attempt may start, so a failed
/// `start_attempt` cannot erase the lane across restart when dynamic directory
/// state is empty or has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRouteRevision {
    pub version: u8,
    pub intent_id: IntentId,
    pub ordinal: u64,
    pub relays: BTreeSet<RelayUrl>,
}

/// Effective attempt state. New v1 rows record `Started` before the engine
/// emits `PublishEvent` and are never rewritten; terminal variants are
/// overlaid from additive details. Upgrade reads also preserve legacy terminal
/// v1 rows written before the additive detail table existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptOutcome {
    Started,
    Acked,
    Rejected(String),
    GaveUp,
    OutcomeUnknown,
}

/// Successful result of making one attempt ordinal terminal. Missing rows
/// and contradictory terminals are errors, never false-success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishAttemptOutcome {
    Committed,
    AlreadySame,
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
    ///
    /// Fallible (issue #122): the ingest door runs on every relay EVENT
    /// frame, so a realistic persistence failure (disk full, I/O error) must
    /// return `Err(PersistenceError)` rather than panic the embedding app.
    /// The redb backend propagates the real redb error; `MemoryStore` never
    /// actually returns `Err` (no I/O). Serde/logic invariant violations
    /// (a corrupt stored row) remain `.expect()`-on-invariant, matching the
    /// durable-write doors' established convention.
    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError>;

    /// Insert a relay-delivery batch in input order. Backends may override
    /// this to amortize durable transaction cost while preserving the exact
    /// per-event governed semantics and outcomes of repeated [`Self::insert`]
    /// calls. The default keeps non-transactional backends source-compatible.
    fn insert_batch(
        &mut self,
        events: Vec<(Event, RelayObserved)>,
    ) -> Result<Vec<InsertOutcome>, PersistenceError> {
        events
            .into_iter()
            .map(|(event, from)| self.insert(event, from))
            .collect()
    }

    /// Query current winners only (never a superseded/stale event), matched
    /// via `nostr::Filter::match_event`, each with its provenance attached.
    /// Fallible for the same reason as [`EventStore::insert`] (issue #122):
    /// a read-path I/O error surfaces as `Err` instead of panicking.
    ///
    /// `filter.limit` is NOT consulted by this LOCAL read path (#124): every
    /// currently-matching row is returned, in no particular order (neither
    /// backend orders its internal candidates by `created_at` — both are
    /// effectively id-keyed), regardless of `limit`. This is DELIBERATE, not
    /// an oversight — honoring `limit` locally requires a `created_at`-desc
    /// ordering + truncation, and choosing that ordering is an owner-
    /// reserved decision (issue #9's app-defined-sort-vs-closed-`OrderKey`
    /// fork, deferred to the Collection Tier-A gate), not something to
    /// settle as a side effect of this fix. Contrast with the WIRE path:
    /// `nmp_grammar::ConcreteFilter::to_nostr` DOES lower `limit` into this
    /// very `filter` before it ever reaches a relay, so a well-behaved
    /// relay caps what it SENDS you — a genuine, honored guarantee. But
    /// that guarantee governs the wire only; it says nothing about what a
    /// LATER local-only call to THIS method returns once the cache holds
    /// more than `limit` matching rows (reconnect replay, multiple relays
    /// each independently capped, etc.) — this method's own answer is
    /// uncapped regardless. Both backends are cross-checked for this exact
    /// contract (`store_contract.rs`); when #9 resolves, whoever implements
    /// ordered/truncated local reads updates that test, not just adds one.
    ///
    /// The app never sees this uncapped answer directly, though: the handle
    /// PROJECTION (`EngineCore::rows_and_evidence_for`, #124 via #139) caps the
    /// app-facing row set to the `limit` most recent by `created_at`
    /// (`EventId`-tiebroken). Persistent stores may use the separate
    /// [`EventStore::query_newest`] door to pre-bound each root atom before
    /// that final merged cap. That is NIP-01 limit-recency SELECTION — WHICH
    /// rows survive — not a display ordering: the app receives an unordered,
    /// `EventId`-keyed `RowDelta` stream and sorts it itself, so #9's
    /// display-sort fork stays open and the two compose. This store door
    /// deliberately stays uncapped so unlimited reactive recompute and
    /// negentropy still see every match. A `Derived` node carrying an explicit
    /// limit uses [`EventStore::query_newest`] instead: its projection is
    /// defined over the selected newest `N`, not over the complete history.
    fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError>;

    /// Return at most `limit` current matches in NIP-01 newest-first
    /// selection order: `created_at` descending, then event id ascending.
    ///
    /// This is a distinct door from [`EventStore::query`], whose deliberately
    /// complete result is required by unlimited reactive recompute and
    /// negentropy. Handle root projections and explicitly limited `Derived`
    /// nodes use this bounded door. The default implementation preserves
    /// backend correctness by sorting the complete answer; persistent backends
    /// may override it with an ordered index scan that stops as soon as
    /// `limit` accepted rows have been found.
    fn query_newest(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        let mut rows = self.query(filter)?;
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Remove `id` from the store — clearing both the id index and, if `id`
    /// is the current replaceable/addressable winner for its address, the
    /// address index too — and hand back the removed row whole, or `None`
    /// if `id` was not held. Engine-facing only (kind:5 processing,
    /// optimistic-write rejection); never a general delete API.
    fn remove(
        &mut self,
        id: EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError>;

    /// Drain every row whose NIP-40 `expiration` is `<= now`, removing each
    /// one (through the same [`EventStore::remove`] door) and returning the
    /// full rows. Index-backed (retraction-and-negative-deltas.md §3.1): a
    /// persistent `(expiry_ts -> {id})` index is maintained on every insert
    /// and every removal, so this drains in `O(log n + due)`, not a full
    /// scan.
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError>;

    /// The earliest NIP-40 `expiration` deadline among currently stored
    /// rows, or `None` if nothing carries one. Index-backed: peeks the
    /// minimum of the same persistent expiration index `expire_due` drains.
    fn next_expiration(&self) -> Option<Timestamp>;

    /// Record that `relay` has proven `proven` for `atom`'s window-erased
    /// shape UNDER its declared `source`/`access` (ruling §1/§3, #106-
    /// widened: the coverage identity is now the full [`ContextualAtom`],
    /// never a bare `ConcreteFilter` alone -- the caller, which knows the
    /// atom's `Demand` context, must supply it; the store has no notion of
    /// `SourceAuthority`/`AccessContext` of its own). Merge-only: no public
    /// lowering path exists outside `gc`.
    fn record_coverage(
        &mut self,
        atom: &ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError>;

    /// The proven interval for `key` at `relay`, or `None` if no row exists.
    /// `None` means this relay has no persisted interval for this key; it
    /// makes no wider claim.
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval>;

    /// Apply an EXPLICIT durable-retention policy by running claim-based GC
    /// (ruling §5): evicts every regular
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
    ///
    /// This is never an ordinary startup, query, shutdown, or implicit
    /// memory-pressure maintenance step. The production engine does not call
    /// this door: verified durable rows are retained by default. A host that
    /// deliberately adopts a quota, disk-pressure, or user-selected retention
    /// policy must make that policy inspectable and invoke this destructive
    /// door explicitly. Query/result/delivery bounds limit resident work; they
    /// are not permission to call `gc` or delete durable history.
    ///
    /// This contract does not promise infinite disk. It makes the transition
    /// from retained history to policy-evicted history explicit, reportable,
    /// and coverage-safe.
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError>;

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
    /// embedding app. As of issue #122 the ingest/read doors above
    /// (`insert`/`query`/`remove`/`expire_due`/`record_coverage`/`gc`) are
    /// fallible on the same footing; only serde/logic invariant violations
    /// (a corrupt persisted row) remain `.expect()`-on-invariant by design.
    /// `MemoryStore` never actually returns `Err` (no I/O).
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError>;

    /// Swap the sentinel signature on `intent_id`'s frozen body for the
    /// real `sig` and flip the canonical `SigState`/`IntentSigState` to
    /// `Signed`, in the SAME transaction that durably drops the intent's
    /// own `OUTBOX_DISPLACED` stash (R6) and updates its retained receipt.
    /// Keyed by `IntentId`, NOT the frozen event's id (architecture review
    /// correction — load-bearing): the intent's `OUTBOX_INTENTS.frozen_json`
    /// is the durable source of truth for its body regardless of whether a
    /// live `EVENTS` row currently exists for it. Three cases, uniformly:
    /// (a) a live row's owner set CONTAINS `intent_id` (issue #2, team-lead
    /// decision: ownership is a SET — an exact `Duplicate` is a CO-OWNER
    /// of the SAME row, not a second row of its own; see `LocalOrigin`'s
    /// doc) — mutate it in place (same id — a NIP-01 id never depends on
    /// `sig` — so this is a value update, not a remove/re-add) — refused
    /// (`NotFound`) if the row's `SigState` is ALREADY `Signed`, even by a
    /// different co-owner, so a later distinct owner's promotion can never
    /// overwrite the one real signature with a second one; (b) no live
    /// row, but `intent_id` is a member of some OTHER intent's
    /// `OUTBOX_DISPLACED` stash entry's owner set (it was superseded by a
    /// later local edit before it could sign) — sync the real signature
    /// into that stash entry too (same already-`Signed` refusal applies),
    /// so a future restore of it never resurrects a stale sentinel copy;
    /// (c) neither (the intent was `Stale`/`Duplicate` at acceptance with
    /// no shared row, or its row was since superseded by a RELAY-observed
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
    /// review correction as `promote_signed`, same three cases, same
    /// ownership-SET model): (a) a live row's owner set CONTAINS
    /// `intent_id` — remove `intent_id` from that set; the row is only
    /// actually `remove(id, Rejected)`-ed (no tombstone) once the set is
    /// EMPTY, `SigState` is still `Pending`, AND no relay has
    /// independently confirmed it (`Provenance::seen` empty) — an exact
    /// `Duplicate`'s still-open obligation, an already-`Signed` state some
    /// OTHER co-owner committed, or independent relay provenance, all
    /// survive this one intent's cancellation (see `LocalOrigin`'s doc);
    /// if actually removed, this intent's durably-stashed `displaced`
    /// predecessor (if any) is then re-`insert`ed through the same one
    /// door — it wins its address back by ordinary supersession, never an
    /// un-supersede operation; (b) no live row, but `intent_id` is a
    /// member of some OTHER intent's `OUTBOX_DISPLACED` stash entry's
    /// owner set — same conditional removal, applied to that stash slot's
    /// owner set instead; (c) neither — nothing to remove or restore in
    /// `EVENTS`. In every case, this intent's own `OUTBOX_INTENTS`/
    /// `OUTBOX_DISPLACED` rows are deleted and its retained receipt
    /// updated to `Compensated`. Fallible for the same reason
    /// `accept_write` is.
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
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError>;

    /// Append the next canonical resolved-route revision for an open intent.
    /// This must commit before any `start_attempt` or wire publication for a
    /// relay in the revision.
    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError>;

    /// Recover every resolved-route revision in ascending ordinal order.
    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError>;

    /// Atomically append the next attempt ordinal for `(intent, relay)` and
    /// its exact signed bytes. This door must return successfully before a
    /// caller may place those bytes on the wire.
    fn start_attempt(
        &mut self,
        intent_id: IntentId,
        relay: RelayUrl,
        event: Event,
    ) -> Result<RecoveredAttempt, PersistenceError>;

    /// Make one started ordinal terminal. The full key prevents a late ACK
    /// from closing a newer attempt.
    fn finish_attempt(
        &mut self,
        intent_id: IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError>;

    /// Read all retained attempt facts for one intent in stable key order.
    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError>;

    /// Idempotently seed every missing lane from bounded route/attempt
    /// ranges. Existing cursors are validated and retained.
    fn bootstrap_outbox_lanes(
        &mut self,
        _intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    fn recover_outbox_lanes(
        &self,
        _intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    /// Read at most `limit` due rows in stable `(time,intent,relay)` order.
    fn due_outbox_deadlines(
        &self,
        _now: Timestamp,
        _limit: usize,
    ) -> Result<Vec<LaneDeadline>, PersistenceError> {
        Err(PersistenceError("outbox deadlines unsupported".into()))
    }

    fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        Err(PersistenceError("outbox deadlines unsupported".into()))
    }

    fn set_lane_waiting(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    fn set_lane_eligible(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    fn set_lane_transient(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _eligible_at: Timestamp,
        _cause: TransientCause,
        _raw_reason: Option<String>,
    ) -> Result<RecoveredLane, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    /// End the current ordinal as a nonterminal wait with no deadline.
    /// The attempt detail and waiting cursor advance atomically, so restart
    /// cannot mistake an AUTH/offline wait for a live ambiguous send.
    #[allow(clippy::too_many_arguments)]
    fn suspend_lane_attempt(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _at: Timestamp,
        _cause: TransientCause,
        _raw_reason: Option<String>,
        _auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    /// Atomically append new immutable v1 Started evidence, additive details,
    /// and advance an eligible cursor to awaiting handoff.
    fn start_lane_attempt(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _event: Event,
        _started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    /// Atomically retain handoff evidence and apply the engine-selected next
    /// fact, maintaining the typed deadline index in the same commit.
    fn record_lane_handoff(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _detail: AttemptHandoffDetail,
        _next: PostHandoffState,
    ) -> Result<RecoveredLane, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    /// Make the current attempt terminal without rewriting its immutable v1
    /// Started row. Exact ordinal + lane revision reject late ACKs against a
    /// newer attempt; detail, cursor, and deadline removal share one commit.
    fn finish_lane_attempt(
        &mut self,
        _key: &LaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _outcome: AttemptOutcome,
        _finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

    fn recover_attempt_details(
        &self,
        _intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttemptDetails>, PersistenceError> {
        Err(PersistenceError(
            "outbox attempt details unsupported".into(),
        ))
    }

    /// Delete bounded open-work rows only after a non-empty lane set is all
    /// terminal. Receipts and all route/attempt/detail evidence are retained.
    fn close_terminal_intent(
        &mut self,
        _intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        Err(PersistenceError("outbox lanes unsupported".into()))
    }

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
