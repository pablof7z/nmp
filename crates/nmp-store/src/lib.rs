//! `nmp-store` — NMP's one concrete durable store, [`RedbStore`]: the one
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
//! Coverage (`record_coverage`/`get_coverage`) implements the store half of
//! `docs/design/query-demand-and-evidence.md` and issue #816's
//! facts-before-claims contract — see [`coverage`] for the full recap.
//! Claim-based bounded GC (`gc`) evicts only regular (non-addressed) events
//! matched by no live claim, lowering any coverage row it invalidates in the
//! same step.
//!
//! Retraction (`docs/design/retraction-and-negative-deltas.md`, issue #28):
//! kind:5 (NIP-09) deletion runs inside `insert` and writes PERMANENT
//! tombstones (§7 owner decision — never GC-claimed) so a later redelivery
//! of a deleted event is `Refused(Tombstoned)`; NIP-40 `expiration` is
//! tracked in a persistent index so `expire_due`/`next_expiration` are
//! index-backed, not O(stored rows).
//!
//! Durable write-delivery (`docs/design/crashsafe-accepted-2-3-plan.md`,
//! issues #2/#3, Fable checkpoint verdict Q2): this crate is now the event
//! **and** publish-queue store in the current Redb implementation — one
//! atomic `redb::Database` boundary. This is an implementation shape, not a
//! requirement that every backend or platform use one physical engine. A
//! split implementation must keep each authority internally atomic, persist
//! control intent before event projection, replay deterministically and
//! idempotently, and reconcile before serving queries or transport. A
//! locally-authored write intent enters through [`RedbStore::accept_write`]
//! (the same dedup/tombstone/supersession rules `insert` runs, stamping
//! local provenance + [`SigState::Pending`] instead of a `RelayObserved`),
//! committing the pending row AND the durable intent/displaced-stash journal
//! in ONE transaction. [`RedbStore::promote_signed`] swaps the real
//! signature in place (zero id churn — a NIP-01 id never depends on `sig`)
//! and durably drops the displaced stash. [`RedbStore::compensate_write`]
//! undoes a pre-signature-terminated intent: `remove(id, Rejected)` (no
//! tombstone — the row was never validly signed) plus a compensating
//! re-`insert` of whatever it displaced, through the same one door.
//! [`RedbStore::recover_publish_queue`] replays every still-open intent after a
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
//! state is retained under `PUBLISH_QUEUE_RECEIPTS`, independently of
//! `PUBLISH_QUEUE_INTENTS`'s open-work row, so [`RedbStore::reattach_receipt`]
//! keeps answering for a terminal receipt after its open-work row is gone
//! (see [`ReceiptState`]'s doc).
//!
//! No store implementation verifies a signature: the one
//! `nostr::Event::verify` call an accepted signer result must pass happens
//! on the caller's side, in `nmp-engine`. What it produces is a
//! [`VerifiedSignature`], the only value `promote_signed` accepts — so the
//! precondition is carried by a type instead of asserted in prose (#768),
//! and the door still binds it to the intent's own frozen id before
//! mutating anything. The engine's send-time attribution snapshots stay out
//! of scope too (this crate only stores whatever interval it is told to
//! record).

mod address_key;
mod binary_event;
mod coverage;
mod coverage_claims;
mod persistence_failure;
mod persistent_store_lifetime;
mod redb_store;
mod semantic_edit;
#[cfg(test)]
mod semantic_oracle;
mod terminal_retention;
#[cfg(test)]
mod terminal_retention_tests;

#[cfg(feature = "bench-instrumentation")]
pub mod ingest_attribution;

pub use coverage::{coverage_key, CoverageInterval, CoverageKey, GcReport, GcRetentionSet};
pub use coverage_claims::coverage_claim_atoms;
pub use persistence_failure::{DurabilityOutcome, PersistenceError, PersistenceFault};
pub use persistent_store_lifetime::{RedbStoreOpenError, RedbStoreResetError};
#[cfg(any(test, feature = "test-instrumentation"))]
pub use redb_store::testing;
#[cfg(any(test, feature = "test-instrumentation"))]
pub use redb_store::OrderedEventReadPause;
pub use redb_store::RedbStore;
#[cfg(feature = "bench-instrumentation")]
pub use redb_store::{
    prepare_equivalent_store_corpus, run_fjall_governed_ingest_bench,
    run_lmdb_governed_ingest_bench, run_packed_postings_bench,
    run_prepared_redb_compact_index_bench, run_prepared_redb_redo_index_bench,
    run_prepared_redb_store_bench, run_prepared_redb_unified_index_bench, run_store_bench_variant,
    FjallGovernedIngestMetrics, LmdbGovernedIngestMetrics, LmdbPackedWork, PackedPostingsBackend,
    PackedPostingsMetrics, PackedQueryMetrics, RedbRedoIndexMetrics, StoreBenchAttribution,
    StoreBenchMetrics, StoreBenchPreparedBatch, StoreBenchPreparedCorpus,
    StoreBenchPreparedMetrics, StoreBenchPreparedRecord, StoreBenchPreparedTable,
    StoreBenchProcessCounters, StoreBenchVariant,
};
pub use semantic_edit::{
    AccessContextId, FiniteSemanticSourceRound, MaterializationCandidate, MaterializationId,
    OperationResolution, OperationSourceRequirement, PendingMaterializationState, QualifiedSource,
    RecoveredSemanticResource, ReplayFormatId, ReplayProgramId, ResolvedOperation, SemanticAccept,
    SemanticCohortClose, SemanticCohortCloseOutcome, SemanticCurrentState,
    SemanticDestinationPlanClosure, SemanticGeneration, SemanticInstallOutcome, SemanticOperation,
    SemanticPlan, SemanticProgramDigest, SemanticRefusal, SemanticRematerialize, SemanticSource,
    SemanticSourceInstall, SemanticSourceMemberState, SemanticSourcePolicy, SemanticSourceRequest,
    SemanticSourceRoundFact, SemanticSourceRoundOutcome, SemanticSourceTerminal, SourceEvidence,
    SourcePlanId, SourceRevision, SourceRoundId, StartingSource, StartingSourceRequirement,
};

use std::collections::{BTreeMap, BTreeSet};

use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, PublicKey, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize};

/// Stable identifier for a durable write intent, ALLOCATED BY THE STORE
/// ITSELF from a durable, monotonically-advancing high-water mark
/// (`PUBLISH_QUEUE_META` for `RedbStore`) bumped inside the SAME `accept_write`
/// transaction that journals the intent — never inferred from the
/// currently-open set.
///
/// This is a load-bearing correction (architecture review, post-initial-
/// build): an earlier revision of this door took a CALLER-assigned
/// `IntentId` and left allocation to `nmp-engine`. That is unsound the
/// moment R8-style terminal cleanup exists: `PUBLISH_QUEUE_INTENTS` rows are
/// deleted once an intent's open work concludes (`compensate_write` today;
/// a future all-lanes-terminal path later), so a caller-side allocator that
/// infers "next free" from the currently-*open* recovered set will
/// eventually reissue an id that a terminated intent already used —
/// colliding with that intent's still-*retained* [`PublishQueueReceipt`] (see
/// [`RedbStore::reattach_receipt`]) or any retained per-relay attempt
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
/// [`RedbStore::accept_write`] rather than [`RedbStore::insert`].
///
/// `owners` is a SET, not a single `IntentId` (architecture review
/// correction, team-lead decision on issue #2): an earlier revision
/// conflated "this row's canonical signature state" with "the one intent
/// that backs it," which broke the moment a byte-identical `Duplicate`
/// intent was accepted against an already-locally-owned row — cancelling
/// the FIRST intent would remove the row out from under a SECOND intent
/// still durably obligated to deliver it (its own `PUBLISH_QUEUE_INTENTS`/receipt
/// stayed open with no canonical row to promote or compensate). Every
/// accepted intent that currently backs this row's existence is a member;
/// coalescing duplicates into one owner was rejected because it would
/// silently drop a later intent's own receipt, violating "every accepted
/// write returns a receipt." `sig_state` stays canonical to the ROW, never
/// per-owner: ANY owner's [`RedbStore::promote_signed`] call sets it, in
/// place, for every owner at once — there is exactly one signature on one
/// row, however many intents are backing it.
///
/// [`RedbStore::compensate_write`] on one owner only removes THAT owner
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
    /// A fresh `Provenance` for a row entering through local acceptance: no
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

    /// Whether a projection pinned to `pinned` may serve this row.
    ///
    /// Two facts that must not be conflated: whether a row APPEARS in a
    /// projection, and whether a relay CARRIED it.
    ///
    /// A FOREIGN row — one that reached this node because some relay
    /// delivered it — answers only for the relays that delivered it. That is
    /// the cross-host isolation a pinned read exists for: one host's cached
    /// rows never answer for a host that did not serve them.
    ///
    /// OUR OWN row is not that case at all. It entered through
    /// [`RedbStore::accept_write`], it is in the outbound publication queue,
    /// and it is ours whatever any relay subsequently does with it.
    /// Withholding it would make every pinned live query lie about what the
    /// user just did, and withdrawing it later would be worse: the feed would
    /// delete the user's own text on the strength of a host it is not even
    /// watching. Its provenance is still reported honestly — the relays that
    /// carried it, which may be none of them, may be all of them, and may be
    /// none of the pinned ones.
    ///
    /// So the distinction is ours versus foreign, spelled `local.is_some()`
    /// — never carried versus uncarried. A row keeps its local origin
    /// forever, including long after relay provenance arrives, which is
    /// exactly the property this needs: publishing to two hosts and watching
    /// one of them must not make the answer depend on the other (#1191).
    /// Empty `seen` remains what it always was, the fact an app's "sending…"
    /// chip resolves off, and it decides nothing here.
    #[must_use]
    pub fn visible_under_pin(&self, pinned: &BTreeSet<RelayUrl>) -> bool {
        visible_under_pin(self.local.is_some(), self.seen.keys(), pinned)
    }
}

/// [`Provenance::visible_under_pin`] for the callers that hold its two
/// inputs without holding a whole [`Provenance`]: a projected committed row,
/// or a persistent backend testing visibility against its index rather than
/// against a decoded row. One rule, one spelling, three call sites.
#[must_use]
pub fn visible_under_pin<'a>(
    ours: bool,
    carried_by: impl IntoIterator<Item = &'a RelayUrl>,
    pinned: &BTreeSet<RelayUrl>,
) -> bool {
    ours || carried_by.into_iter().any(|relay| pinned.contains(relay))
}

/// The sentinel signature every pending row's frozen body carries until
/// [`RedbStore::promote_signed`] swaps in the real one (Fable checkpoint
/// Q1, APPROVED): a NIP-01 id is `hash([0,pubkey,created_at,kind,tags,
/// content])` — the signature is not an id input — so an all-zero 64-byte
/// value round-trips through `nostr::Event`/JSON/`Filter::match_event`
/// unverified (schnorr `Signature` parsing is length-checked only) and the
/// id is final before a real signature exists.
pub fn sentinel_signature() -> Signature {
    Signature::from_slice(&[0u8; 64])
        .expect("64 zero bytes is always a structurally valid (length-checked) schnorr signature")
}

/// The only thing [`RedbStore::promote_signed`] accepts (#768): a
/// signature that a `nostr::Event::verify` call actually passed, carried
/// together with the [`EventId`] it was verified against.
///
/// `promote_signed` used to take a bare `Signature` plus a doc sentence
/// telling the caller to have verified it first. A sentence is not a guard:
/// any store consumer could hand the door a signature belonging to a
/// different event, or to no event at all, and the production stores would
/// still replace the sentinel, flip every co-owner to `Signed`, drop the
/// displaced recovery stash, and — for a pending kind:5 draft — turn
/// provisional suppression claims into PERMANENT tombstones. That is the
/// convention-only failure class `docs/bug-class-ledger.md:3-5` rules out,
/// and the precondition the Destructive-API Gate requires be typed.
///
/// The fields are private and [`Self::verify`] is the only constructor, so
/// the value cannot exist unless one verification succeeded. `Event::verify`
/// recomputes the NIP-01 id from the body and checks the schnorr signature
/// against THAT id and pubkey, so [`Self::event_id`] is not a label the
/// caller chose — it is the identity the signature actually covers. The
/// store compares it to the intent's own durable frozen id, which is what
/// binds the evidence to *this* write rather than to merely some valid one.
///
/// Verification stays a caller-side act performed exactly once (#387): the
/// engine's signer-result validation constructs this value and hands it
/// down, and no store implementation runs a second Schnorr check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedSignature {
    event_id: EventId,
    signature: Signature,
}

impl VerifiedSignature {
    /// Verify `event` whole — id recomputed from the body, schnorr
    /// signature checked against that id and `event.pubkey` — and keep the
    /// proof. `Err` is `nostr`'s own verification failure, unchanged.
    pub fn verify(event: &Event) -> Result<Self, nostr::event::Error> {
        event.verify()?;
        Ok(Self {
            event_id: event.id,
            signature: event.sig,
        })
    }

    /// The id the signature was verified against. A store door matches this
    /// against the intent's frozen id before it mutates anything.
    #[must_use]
    pub fn event_id(&self) -> EventId {
        self.event_id
    }

    /// The verified signature itself — what actually replaces
    /// [`sentinel_signature`] on the canonical row.
    #[must_use]
    pub fn signature(&self) -> Signature {
        self.signature
    }
}

/// Re-freeze `frozen` at `created_at`, re-deriving its NIP-01 id over the
/// stamped body. Used by the acceptance transaction to apply
/// [`AcceptWrite::monotonic_stamp`] against the row it is CAS-ing; the
/// signature stays [`sentinel_signature`] because the body is still
/// pre-signature at this point (which is precisely why moving the stamp
/// here is possible at all).
pub(crate) fn restamped(frozen: &Event, created_at: Timestamp) -> Event {
    Event::new(
        EventId::new(
            &frozen.pubkey,
            &created_at,
            &frozen.kind,
            &frozen.tags,
            &frozen.content,
        ),
        frozen.pubkey,
        created_at,
        frozen.kind,
        frozen.tags.clone(),
        frozen.content.clone(),
        frozen.sig,
    )
}

/// A stored event plus its provenance. What `query` returns — every caller
/// gets provenance for free, never a bare `Event` (ledger #5's falsifier:
/// no `query` path returns an event without its provenance populated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub event: Event,
    pub provenance: Provenance,
}

/// The closed canonical continuation key for newest-first event selection.
///
/// Store pages are ordered by `created_at` descending, then event id
/// ascending. A cursor is exclusive: the next page may contain exactly rows
/// whose timestamp is lower, or whose timestamp is equal and id is greater.
/// Keeping both protocol facts in one typed key prevents callers from
/// approximating a continuation by decrementing Nostr's one-second timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventCursor {
    pub created_at: Timestamp,
    pub event_id: EventId,
}

impl EventCursor {
    pub const fn new(created_at: Timestamp, event_id: EventId) -> Self {
        Self {
            created_at,
            event_id,
        }
    }

    pub fn from_event(event: &Event) -> Self {
        Self::new(event.created_at, event.id)
    }
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

/// The result of a [`RedbStore::insert`] call.
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
        /// Whether the row this delivery merged into is one this node
        /// accepted itself (`Provenance.local.is_some()`). A relay echo never
        /// changes it, and it is not `!satisfied_intents.is_empty()`: an
        /// already-signed row, and a row whose owners were all compensated
        /// away, satisfy no intent and are still ours. Pinned projections
        /// need it to evaluate [`Provenance::visible_under_pin`] over the
        /// committed delta without re-reading the row.
        locally_accepted: bool,
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

/// Why a [`RedbStore::insert`] refused an event outright, before it ever
/// touched an index.
///
/// Serialized because most semantic refusals of a locally-authored write are
/// retained as one permanently-failed receipt through
/// [`RedbStore::accept_refused`]. [`RefuseReason::AlreadyExpired`] is the
/// deliberate exception: the engine refuses it before custody, so it creates
/// no receipt, event body, signer request, route, lane, or attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Why a [`RedbStore::remove`] call is removing a row. Exists so
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

/// Journal-level signature state of an `PUBLISH_QUEUE_INTENTS` row (Fable
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
    /// [`RedbStore::promote_signed`] has run; the row carries a real
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
/// id colliding with a retained `PUBLISH_QUEUE_RECEIPTS` row, making
/// `reattach_receipt` ambiguous.
pub struct AcceptWrite {
    /// The one accepted write payload. Replaceable operations deliberately
    /// carry no event body: the body becomes authoritative only when a
    /// materialization transition installs it into the canonical event table.
    pub payload: AcceptWritePayload,
    /// The frozen, unsigned NIP-01 body: pubkey/created_at/kind/tags/
    /// content are final and `event.id` is already `EventId::new(..)` over
    /// exactly those fields (the signature is not an id input — Q1).
    /// `event.sig` must be [`sentinel_signature`] until
    /// [`RedbStore::promote_signed`] swaps in the real one.
    /// The pinned signing identity (#43 "pins the chosen identity at
    /// acceptance"). Ordinarily equal to `frozen.pubkey`; kept as an
    /// explicit field because it is a distinct journal fact (#2's "expected
    /// pubkey"), not merely derivable convenience.
    pub expected_pubkey: PublicKey,
    /// Opaque placeholder the store persists and returns verbatim — #47
    /// gives it real meaning; this frame only pins the persistence hook
    /// (Fable checkpoint Q5).
    pub signing_identity_ref: String,
    pub accepted_at: Timestamp,
    /// #591 crash-safe correlation token. When `Some`, checked (and, on a
    /// first sighting, journaled) inside this SAME acceptance transaction
    /// -- see [`RedbStore::accept_write`]'s doc for the exact protocol.
    pub correlation: Option<nmp_grammar::CorrelationToken>,
}

/// The closed work shape accepted through [`RedbStore::accept_write`].
/// There is no optional semantic sidecar beside an authoritative event body.
pub enum AcceptWritePayload {
    Event {
        frozen: Box<Event>,
        replaceable_base: Option<Option<EventId>>,
        monotonic_stamp: bool,
        routing: String,
        sig_state: IntentSigState,
    },
    ReplaceableOperation(Box<SemanticAccept>),
}

/// The result of an [`RedbStore::accept_write`] call — mirrors
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
/// `PUBLISH_QUEUE_DISPLACED`), and made an exact-`Duplicate` kind:5 intent's
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
    /// A replaceable operation failed a typed store precondition before
    /// custody. No intent, receipt, operation row, or counter is allocated.
    ReplaceableOperationRefused(SemanticRefusal),
    /// A bodyless replaceable operation entered the ordinary intent/receipt
    /// journal. `current` is progressive and may contain no materialization.
    ReplaceableOperation {
        intent_id: IntentId,
        receipt_id: u64,
        current: SemanticCurrentState,
        installed: Option<Box<StoredEvent>>,
        predecessor: Option<Box<StoredEvent>>,
    },
    /// Brand-new pending row, no address competition. `intent_id`/
    /// `receipt_id` are the store-allocated ids (see [`IntentId`]'s doc) —
    /// the ONLY place a caller learns either.
    Inserted {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
    },
    /// This exact event id was already held with local provenance (an edge
    /// case, not the relay-echo hand-off, which goes through
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
    /// `replaced` — durably stashed by the caller into `PUBLISH_QUEUE_DISPLACED`
    /// in the SAME transaction, so pre-signature compensation
    /// (`compensate_write`) can restore it (retraction doc §4.2).
    Superseded {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
        replaced: Box<StoredEvent>,
        /// Older open delivery obligations at this exact address that were
        /// retired atomically with this acceptance. Each item says whether a
        /// bounded safety receipt remains necessary because its bytes may
        /// have crossed the local transport handoff.
        retired: Vec<RetiredIntent>,
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

/// One older replaceable/addressable obligation atomically retired when a
/// newer winner was accepted. This is acceptance evidence for the engine,
/// not another app-facing workload noun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredIntent {
    pub intent_id: IntentId,
    pub receipt_id: u64,
    /// True only when local transport evidence cannot prove the obsolete
    /// bytes stayed local. This selects the honest app-facing terminal and
    /// the bounded durable safety receipt.
    pub handoff_may_have_occurred: bool,
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
            | AcceptOutcome::Kind5Processed { intent_id, .. }
            | AcceptOutcome::ReplaceableOperation { intent_id, .. } => Some(*intent_id),
            AcceptOutcome::ReplaceableOperationRefused(_) | AcceptOutcome::Refused(_) => None,
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
            | AcceptOutcome::Kind5Processed { receipt_id, .. }
            | AcceptOutcome::ReplaceableOperation { receipt_id, .. } => Some(*receipt_id),
            AcceptOutcome::ReplaceableOperationRefused(_) | AcceptOutcome::Refused(_) => None,
        }
    }

    /// The canonical row this acceptance is about, when it produced one.
    ///
    /// Its `event` is the body the store actually froze — which is not
    /// always the body the caller handed in: an [`AcceptWrite`] with
    /// `monotonic_stamp` set may have had its `created_at` moved forward
    /// inside the transaction, re-deriving the id. A caller that needs the
    /// frozen body (to hand it to a signer, to name it on a receipt) must
    /// read it from here rather than from what it sent.
    ///
    /// `None` for `Stale` — which lost its address race and owns no row —
    /// and for `Refused`, which journaled nothing. Neither is reachable for
    /// a `monotonic_stamp` write whose precondition passed: the stamp is
    /// strictly greater than the winner it was compared against, so the
    /// candidate cannot then lose to that same winner.
    pub fn accepted_row(&self) -> Option<&StoredEvent> {
        match self {
            AcceptOutcome::Inserted { row, .. }
            | AcceptOutcome::Duplicate { row, .. }
            | AcceptOutcome::Superseded { row, .. }
            | AcceptOutcome::Kind5Processed { row, .. } => Some(row),
            AcceptOutcome::ReplaceableOperation { .. }
            | AcceptOutcome::Stale { .. }
            | AcceptOutcome::ReplaceableOperationRefused(_)
            | AcceptOutcome::Refused(_) => None,
        }
    }
}

/// The result of an [`RedbStore::promote_signed`] call — keyed by
/// `IntentId`, not the frozen event's id (architecture review correction: a
/// `Duplicate`/`Stale` intent with no shared row never won a live row at
/// its own id at all, and a once-live row can since have been superseded,
/// kind:5-deleted, or expired). Three cases, all reachable: `intent_id` is
/// a MEMBER of a live row's owner set (issue #2, team-lead decision —
/// ownership is a SET, so an exact `Duplicate` sharing an already-locally-
/// owned row is a CO-OWNER of it, not a row of its own) — sentinel swapped
/// for `sig` in place, same id, same EVENTS/ADDR_INDEX/BY_AUTHOR/BY_KIND/BY_TAG
/// entries, zero churn; `intent_id` is a member of some OTHER intent's
/// `PUBLISH_QUEUE_DISPLACED` stash entry's owner set (chained local supersession
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
/// co-owner's own `PUBLISH_QUEUE_INTENTS`/`PUBLISH_QUEUE_RECEIPTS` row to `Signed`
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
/// `PUBLISH_QUEUE_DISPLACED` stash for `intent_id` AND every co-owner named in
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
    MaterializationPromoted {
        row: Box<StoredEvent>,
        members: Vec<IntentId>,
    },
    /// Exact replaceable-operation CAS witness no longer names current state.
    /// No row, journal, or receipt changed.
    Stale,
    /// This `IntentId` names no still-open intent, OR its OWN journal is
    /// ALREADY `Signed` — either because it promoted before (codex-nova's
    /// original repeat-promotion finding), or because some OTHER co-owner
    /// promoted first and this call's `co_signed` already advanced it
    /// (this intent's own delayed signer callback arriving after the
    /// fact). Also covers already compensated, or never accepted through
    /// `accept_write`.
    NotFound,
}

/// The result of an [`RedbStore::compensate_write`] call — keyed by
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
/// intent's `PUBLISH_QUEUE_INTENTS`/`PUBLISH_QUEUE_DISPLACED`/suppression-claim rows
/// were all deleted in the same transaction. Boxed for the same reason
/// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
/// small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensateOutcome {
    Compensated {
        restored: Option<Box<StoredEvent>>,
        revealed: Vec<StoredEvent>,
    },
    /// The intent crossed signature promotion; the destructive pre-signature
    /// door refuses without changing its row, receipt, or lanes.
    AlreadySigned,
    /// This `IntentId` names no still-open intent: already compensated or
    /// never accepted through `accept_write`.
    NotFound,
}

/// Typed result from the receipt-keyed queue-entry removal door
/// ([`RedbStore::remove_publish_queue_entry`], #1039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveQueueEntryOutcome {
    Removed,
    NotFound,
    /// The receipt still owns an open `PUBLISH_QUEUE_INTENTS` row. Removal is
    /// for entries nothing is going to move; an intent with live work is
    /// cancelled, not removed.
    StillOpen,
}

/// One still-open intent replayed by [`RedbStore::recover_publish_queue`] on
/// boot. The pending row itself is NOT re-inserted — it is already live in
/// the store (committed atomically at `accept_write` time) and query-visible
/// from the first post-boot subscription; this is only the journal metadata
/// `nmp-engine` needs to rebuild its in-memory `PendingWrite`/
/// `event_to_receipt` bookkeeping (plan §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueIntent {
    pub intent_id: IntentId,
    pub receipt_id: u64,
    pub work: PublishQueueWork,
    pub expected_pubkey: PublicKey,
    pub signing_identity_ref: String,
    /// The predecessor this intent displaced, if any — still durable
    /// (`PUBLISH_QUEUE_DISPLACED` is deleted only by `promote_signed` or
    /// `compensate_write`, never by `recover_publish_queue`), so a post-restart
    /// cancellation can still restore it.
    pub accepted_at: Timestamp,
}

impl PublishQueueIntent {
    /// Project ordinary event work without weakening the closed event-vs-operation shape.
    pub fn event_work(&self) -> Option<(&Event, Option<&StoredEvent>, &str, IntentSigState)> {
        match &self.work {
            PublishQueueWork::Event {
                frozen,
                displaced,
                routing,
                sig_state,
            } => Some((frozen, displaced.as_deref(), routing, *sig_state)),
            PublishQueueWork::ReplaceableOperation { .. } => None,
        }
    }
}

/// Durable open work reconstructed through the one publish queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishQueueWork {
    Event {
        frozen: Event,
        displaced: Option<Box<StoredEvent>>,
        routing: String,
        sig_state: IntentSigState,
    },
    ReplaceableOperation {
        coordinate: nostr::nips::nip01::Coordinate,
        materialization: Option<MaterializationWork>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationWork {
    pub receipt: MaterializationReceipt,
    pub routing: String,
}

/// A durably-retained receipt's coarse status — the STORE-OBSERVABLE
/// subset of the full receipt stream (`nmp-engine`'s `WriteFact` owns
/// the complete enum, including per-relay `Routed`/`Sent`/`Acked`/
/// `Rejected`/`GaveUp`/`Failed`; this crate only knows what its OWN four
/// doors did to a receipt). Retained under `PUBLISH_QUEUE_RECEIPTS` — separately
/// from `PUBLISH_QUEUE_INTENTS`'s open-work row — precisely so a receipt stays
/// reattachable via [`RedbStore::reattach_receipt`] after the open-work
/// row is gone (architecture review correction: R8-style terminal cleanup
/// of `PUBLISH_QUEUE_INTENTS` must never also delete receipt identity/state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptState {
    /// `accept_write` ran; nothing else has happened to this receipt yet.
    Accepted,
    /// `promote_signed` ran; the row carries a real signature. (Per-relay
    /// delivery evidence beyond this point is a later unit's job — the
    /// durable attempt table this frame only creates the schema for.)
    Signed,
    /// `compensate_write` ran; the pending row was retracted pre-signature
    /// (retraction doc §4.2). Terminal — a compensated intent never
    /// promotes.
    Compensated,
    /// The app explicitly cancelled this still-unsigned obligation. The
    /// compensation transaction committed, so this is a durable terminal
    /// fact rather than a generic failure string.
    Cancelled,
    /// A newer accepted event won the same NIP-01 replaceable/addressable
    /// coordinate. Terminal: the obsolete body and all delivery machinery are
    /// gone. A receipt remains only when a local handoff may have occurred;
    /// that safety evidence shares the same private terminal-history FIFO as
    /// every other completed receipt.
    Superseded,
    /// Routing finished — knowledge exhausted — and named zero relays, so
    /// there was nowhere to publish ([`RedbStore::close_unroutable_intent`]).
    /// Terminal, and distinct from [`Self::Refused`]: the instruction was
    /// fine and the store took it; the WORLD had no destination for it.
    /// Retained so a reattaching app is told that, rather than told nothing.
    NoDestination,
    /// The acceptance instruction was answered with a semantic no
    /// ([`RedbStore::accept_refused`]): the store was working and said no.
    /// Terminal at birth — there was never an intent, a journal row, a
    /// signer request or a relay write, only this one retained receipt.
    ///
    /// The write is still in CUSTODY: the app reads the reason back through
    /// reattachment or enumeration, and a
    /// [`RefuseReason::ReplaceableBaseChanged`] carries both event ids so
    /// the app can fetch what is actually there, reapply the user's change
    /// and resubmit without ever troubling them.
    Refused(RefuseReason),
}

/// A durably-retained receipt record, independent of whether the intent's
/// open-work row (`PUBLISH_QUEUE_INTENTS`/[`PublishQueueIntent`]) still exists —
/// see [`ReceiptState`]'s doc for why this separation exists. Complete history
/// remains available while retained; `nmp-store` eventually removes the whole
/// terminal closure under one private global policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueReceipt {
    pub receipt_id: u64,
    /// `Some` for a receipt backed by a real (open or since-closed)
    /// `accept_write` intent. `None` for a receipt-ONLY record
    /// ([`RedbStore::accept_refused`]): a write refused at the acceptance
    /// door still enters custody as one retained, reattachable receipt,
    /// without ever gaining a journal row, a pending event row, a signer
    /// request or a relay write.
    pub intent_id: Option<IntentId>,
    pub expected_pubkey: PublicKey,
    /// Acceptance time for receipts backed by a real write intent. Receipt-
    /// only semantic refusals enter receipt custody but have no accepted
    /// intent, so their private terminal-retention clock is stored separately.
    pub accepted_at: Option<Timestamp>,
    pub payload: PublishQueueReceiptPayload,
}

impl PublishQueueReceipt {
    /// Project the event arm's durable state. Replaceable-operation receipts
    /// have their own closed state vocabulary and therefore return `None`.
    pub fn event_state(&self) -> Option<ReceiptState> {
        match &self.payload {
            PublishQueueReceiptPayload::Event { state, .. } => Some(*state),
            PublishQueueReceiptPayload::ReplaceableOperation { .. } => None,
        }
    }

    /// Project the event arm's exact event id without inventing one for a
    /// bodyless replaceable operation.
    pub fn event_id(&self) -> Option<EventId> {
        match &self.payload {
            PublishQueueReceiptPayload::Event { event_id, .. } => Some(*event_id),
            PublishQueueReceiptPayload::ReplaceableOperation { acceptance, .. } => {
                acceptance.event_id()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueReceiptPayload {
    Event {
        event_id: EventId,
        state: ReceiptState,
    },
    ReplaceableOperation {
        coordinate: nostr::nips::nip01::Coordinate,
        acceptance: ReplaceableOperationAcceptance,
        state: ReplaceableOperationReceiptState,
    },
}

/// Stable acceptance identity for a replaceable-operation receipt.
///
/// The closed bodyless arm preserves the lower store mechanism without
/// inventing an event id. A body-complete accepted receipt owns one immutable
/// event id even while its progressive `current` materialization later moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplaceableOperationAcceptance {
    Bodyless,
    BodyComplete(EventId),
}

impl ReplaceableOperationAcceptance {
    #[must_use]
    pub fn event_id(self) -> Option<EventId> {
        match self {
            Self::Bodyless => None,
            Self::BodyComplete(event_id) => Some(event_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplaceableOperationReceiptState {
    Contributing {
        current: Option<MaterializationReceipt>,
    },
    Settled,
    Resolved,
    Cancelled,
    Refused(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializationReceipt {
    pub materialization: MaterializationRef,
    pub sig_state: IntentSigState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializationRef {
    pub materialization_id: MaterializationId,
    pub event_id: EventId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceableMaterializationTarget {
    pub coordinate: nostr::nips::nip01::Coordinate,
    pub expected_source_revision: SourceRevision,
    pub expected_program_digest: SemanticProgramDigest,
    pub expected_materialization: MaterializationId,
    pub expected_event_id: EventId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionTarget {
    Event(IntentId),
    ReplaceableMaterialization(Box<ReplaceableMaterializationTarget>),
}

/// Versioned, durable evidence for one publication attempt. The key is the
/// full `(intent, relay, ordinal)` tuple: a restart can never confuse a new
/// send with an older ambiguous send, and the exact signed bytes are retained
/// rather than reconstructed from mutable routing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueAttempt {
    pub version: u8,
    pub intent_id: IntentId,
    pub relay: RelayUrl,
    pub ordinal: u64,
    pub event: Event,
    pub outcome: PublishQueueAttemptOutcome,
}

/// Stable identity of one durable publication lane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PublishQueueLaneKey {
    pub intent_id: IntentId,
    /// Exact immutable bytes this lane publishes. A successor generation for
    /// the same semantic operation owner and relay is a different lane; its
    /// ACK, retry, and deadline can never alias the predecessor.
    pub event_id: EventId,
    pub relay: RelayUrl,
}

/// The current, versioned cursor for one `(intent, relay)` obligation.
/// History remains in the route/attempt/detail tables; this is the bounded
/// authoritative row recovery and scheduling read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueLane {
    pub version: u8,
    pub key: PublishQueueLaneKey,
    pub revision: u64,
    pub last_ordinal: u64,
    pub state: PublishQueueLaneState,
}

/// The typed source of a terminal authentication refusal.
///
/// This vocabulary is deliberately source-neutral: a local policy or signer
/// refusal is not a relay rejection merely because it prevents a relay write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthDenialSource {
    Policy,
    Signer,
    Relay,
}

/// Durable authentication-refusal evidence owned by one exact write lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDenial {
    pub source: AuthDenialSource,
    pub reason: String,
}

/// Terminal lane vocabulary.
///
/// Unlike an attempt terminal, a true AUTH denial can finish a lane before
/// the first EVENT attempt exists (ordinal zero). Keeping this separate from
/// [`PublishQueueAttemptOutcome`] makes `Started` structurally impossible in a terminal
/// lane and avoids inventing an attempt merely to retain a denial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueTerminalOutcome {
    Acked,
    Rejected(String),
    GaveUp,
    AuthDenied(AuthDenial),
}

impl PublishQueueTerminalOutcome {
    fn from_attempt(outcome: PublishQueueAttemptOutcome) -> Result<Self, PersistenceError> {
        match outcome {
            PublishQueueAttemptOutcome::Started => Err(PersistenceError::invariant(
                "Started is not a terminal lane outcome",
            )),
            PublishQueueAttemptOutcome::Acked => Ok(Self::Acked),
            PublishQueueAttemptOutcome::Rejected(reason) => Ok(Self::Rejected(reason)),
            PublishQueueAttemptOutcome::GaveUp => Ok(Self::GaveUp),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueLaneState {
    WaitingConnection,
    WaitingAuth,
    Eligible {
        since: Timestamp,
    },
    InFlight {
        ordinal: u64,
        phase: PublishQueueInFlightPhase,
    },
    Transient {
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    },
    Terminal {
        ordinal: u64,
        outcome: PublishQueueTerminalOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueInFlightPhase {
    AwaitingHandoff,
    AwaitingAck { deadline: Timestamp },
}

/// Ordered deadline-index discriminator. Retry eligibility and ACK timeout
/// share one index but remain impossible to conflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueDeadlineKind {
    RetryEligible,
    AckTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueDeadline {
    pub at: Timestamp,
    pub key: PublishQueueLaneKey,
    pub lane_revision: u64,
    pub kind: PublishQueueDeadlineKind,
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
pub enum PublishQueueTransientCause {
    Interrupted,
    AckTimeout,
    ConnectionLost,
    RelayRateLimited,
    RelayError,
    AuthRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueAttemptHandoff {
    pub at: Timestamp,
    pub result: HandoffEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueAttemptTransient {
    pub eligible_at: Timestamp,
    pub cause: PublishQueueTransientCause,
    pub raw_reason: Option<String>,
}

/// The current evidence row beside an immutable `Started` attempt row. Every
/// attempt in the current schema has exactly one of these; there is no
/// pre-detail attempt shape to adopt or synthesize a shell for (#867).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueAttemptDetails {
    pub version: u8,
    pub intent_id: IntentId,
    pub relay: RelayUrl,
    pub ordinal: u64,
    pub started_at: Option<Timestamp>,
    pub handoff: Option<PublishQueueAttemptHandoff>,
    #[serde(default)]
    pub transient: Option<PublishQueueAttemptTransient>,
    pub finished_at: Option<Timestamp>,
    pub terminal: Option<PublishQueueAttemptOutcome>,
}

pub(crate) fn attempt_is_live(
    attempt: &PublishQueueAttempt,
    details: Option<&PublishQueueAttemptDetails>,
) -> bool {
    if attempt.outcome != PublishQueueAttemptOutcome::Started {
        return false;
    }
    match details {
        Some(details) if details.terminal.is_some() || details.transient.is_some() => false,
        Some(details)
            if matches!(
                details.handoff,
                Some(PublishQueueAttemptHandoff {
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

/// Whether a started attempt still carries any possibility that its bytes
/// crossed the local transport boundary. Missing detail is ambiguous after a
/// crash; an explicit `NotHandedOff` is the one definitive negative fact.
pub(crate) fn handoff_may_have_occurred(handoff: Option<&PublishQueueAttemptHandoff>) -> bool {
    !matches!(
        handoff,
        Some(PublishQueueAttemptHandoff {
            result: HandoffEvidence::NotHandedOff,
            ..
        })
    )
}

/// Caller-selected post-handoff persistence state. This is a fact-writing
/// vocabulary, not a classification policy: the engine chooses the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishQueuePostHandoffState {
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
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    },
    Terminal {
        outcome: PublishQueueAttemptOutcome,
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
/// attempt-start cannot erase the lane across restart when dynamic directory
/// state is empty or has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueRouteRevision {
    pub version: u8,
    pub intent_id: IntentId,
    pub ordinal: u64,
    pub relays: BTreeSet<RelayUrl>,
}

/// Effective attempt state. Base rows record `Started` before the engine emits
/// `PublishEvent` and are never rewritten; terminal variants are overlaid from
/// the required detail row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueAttemptOutcome {
    Started,
    Acked,
    Rejected(String),
    GaveUp,
}
