//! [`MemoryStore`] — the in-memory `EventStore`, and the test ORACLE that
//! `RedbStore` is diffed against for every shared contract test
//! (`nmp-store/tests/store_contract.rs`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nostr::filter::MatchEventOptions;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, Filter, Kind, PublicKey, RelayUrl, Timestamp};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins, AddressKey};
use crate::coverage::{
    coverage_key, merge_interval, shape_matches, shrink_after_eviction, window_erase,
};
use crate::{
    AcceptOutcome, AcceptWrite, AttemptHandoffDetail, AttemptOutcome, AttemptTransientDetail,
    ClaimSet, CloseIntentOutcome, CompensateOutcome, CoverageInterval, CoverageKey, DeadlineKind,
    EventStore, FinishAttemptOutcome, GcReport, InFlightPhase, InsertOutcome, IntentId,
    IntentSigState, LaneDeadline, LaneKey, LaneState, LocalOrigin, PersistenceError,
    PostHandoffState, PromoteOutcome, Provenance, ReceiptState, RecoveredAttempt,
    RecoveredAttemptDetails, RecoveredIntent, RecoveredLane, RecoveredReceipt,
    RecoveredRouteRevision, RefuseReason, RelayObserved, RetractReason, SigState, StoredEvent,
    TransientCause, WriteDurability,
};

/// One `OUTBOX_INTENTS` row (M3 durable-outbox unit, crashsafe-accepted-2-3-
/// plan.md §2.2) as retained in memory. `MemoryStore` implements the same
/// door SEMANTICS as `RedbStore` so the two backends can never diverge on
/// the outbox contract (this struct is the in-memory mirror of
/// `RedbStore`'s `OUTBOX_INTENTS` JSON record) — but carries no durability
/// guarantee of its own (Fable checkpoint Q4): `recover_outbox` always
/// returns empty, because nothing here survives a process crash by
/// construction. Its fields are therefore write-only from this backend's
/// own perspective (never read back by `MemoryStore` itself, only kept in
/// lockstep with what `accept_write`/`promote_signed` would persist on
/// `RedbStore`) — `#[allow(dead_code)]` records that deliberately, rather
/// than dropping the fields and letting the two backends' journal shapes
/// silently diverge.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct OutboxIntentRecord {
    receipt_id: u64,
    frozen: Event,
    expected_pubkey: PublicKey,
    signing_identity_ref: String,
    durability: WriteDurability,
    routing: String,
    sig_state: IntentSigState,
    accepted_at: Timestamp,
}

/// A single provisional kind:5 suppression claim (architecture review
/// requirement — codex-nova's suppression-claim model, replacing a
/// withdrawn design that physically moved a target row into a per-intent
/// stash: that made the target's OWN `promote_signed`/`compensate_write`
/// blind to it, since neither searches a stash, and made an exact-
/// `Duplicate` kind:5 intent's promotion unsound — it committed a real
/// deletion with no stash of its own to reverse if something went wrong).
/// A claim names EITHER an e-tag id target (keyed exactly like
/// `deleted_ids`: `(target id, claiming author)`, so a future arrival at
/// that id is only ever suppressed if its real author — fixed by the id's
/// hash — matches) OR an a-tag address target (issue #61 P0 correction:
/// MUST carry the same NIP-09 `created_at` ceiling the permanent
/// `deleted_addrs` mechanism uses — a claim with no ceiling would hide
/// every future winner at that address forever, including one created
/// AFTER the deletion, which is not what NIP-09 authorizes even
/// provisionally). `deleting_author` is carried for diagnostic parity with
/// `TombstoneRecord` — authorization for an address claim is already
/// checked immediately at claim-creation time (`coord.public_key ==
/// deleting.pubkey`), so the address alone is enough to enforce it; the
/// ceiling is what makes visibility correct. NEVER moves or removes the
/// row it names — see `MemoryStore::suppress_by_id`/`suppress_by_addr`'s
/// doc for how visibility is decided.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SuppressClaim {
    Id(EventId, PublicKey),
    Addr(AddressKey, Timestamp, PublicKey),
}

/// An address-tombstone's durable fact: which kind:5 event set the
/// deletion ceiling, and (diagnostics only — the ceiling comparison alone
/// decides refusal) that kind:5's own author. Retention is PERMANENT
/// (retraction-and-negative-deltas.md §7 owner decision) — never GC-claimed.
/// Id-tombstones do NOT use this: see `MemoryStore::deleted_ids`'s doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TombstoneRecord {
    deleting_event_id: EventId,
    deleting_author: PublicKey,
}

/// One coverage row as retained in memory: the window-erased shape it was
/// recorded against (needed so `gc` can test "does an evicted event match
/// this row" — see `crate::coverage::ShapeRecord`'s doc comment for why the
/// shape, not just its hash, must be retained) plus the proven interval.
#[derive(Debug, Clone)]
struct CoverageRow {
    shape: ConcreteFilter,
    interval: CoverageInterval,
}

/// An in-memory `EventStore`. Holds exactly the currently-reachable events:
/// every "regular" (non-replaceable, non-addressable) event ever inserted,
/// plus the current winner (only) for every replaceable/addressable
/// address — each carrying its merged provenance — plus coverage rows keyed
/// by `(CoverageKey, RelayUrl)`. No persistence (that is `RedbStore`'s job);
/// this store is the oracle every persistent-backend test result is diffed
/// against.
#[derive(Debug, Default)]
pub struct MemoryStore {
    by_id: HashMap<EventId, StoredEvent>,
    addr_index: HashMap<AddressKey, EventId>,
    coverage: HashMap<(CoverageKey, RelayUrl), CoverageRow>,
    /// Permanent kind:5 tombstones for individual event ids
    /// (retraction-and-negative-deltas.md §2/§7), keyed `(target id,
    /// deleting author)` -- value is the deleting kind:5's own id
    /// (diagnostics only). NOT collapsed to one record per target id: the
    /// target's real author is unknown until it actually arrives, so an
    /// unauthorized third party can always name an id that's already been
    /// (or will be) legitimately deleted by its real author. If a single
    /// slot per id were overwritable, that unauthorized kind:5 would
    /// silently replace -- and so undo -- the real author's permanent,
    /// authorized deletion the moment the real target is redelivered. Every
    /// distinct claiming author gets its own permanent entry instead; a
    /// redelivered target is refused iff ITS OWN author is among the
    /// claimants, regardless of how many other (irrelevant) authors also
    /// named that id. Never GC-claimed.
    deleted_ids: HashMap<(EventId, PublicKey), EventId>,
    /// Permanent kind:5 tombstones for replaceable/addressable addresses:
    /// the highest deleting-event `created_at` seen for that address (the
    /// "ceiling") plus the record of the deletion that set it. A candidate
    /// with `created_at <= ceiling` is tombstoned; NIP-09 allows a fresh
    /// post-deletion event at the same address to win normally.
    deleted_addrs: HashMap<AddressKey, (Timestamp, TombstoneRecord)>,
    /// Persistent NIP-40 expiration index: `expiry_ts -> {ids expiring at
    /// that instant}`, kept in lockstep with `by_id` on every insert and
    /// every removal so `expire_due`/`next_expiration` never rescan the
    /// whole store (retraction-and-negative-deltas.md §3.1).
    expiration_index: BTreeMap<Timestamp, HashSet<EventId>>,
    /// `OUTBOX_INTENTS` mirror (crashsafe-accepted-2-3-plan.md §2.2) — one
    /// entry per still-open locally-accepted write intent.
    outbox_intents: HashMap<IntentId, OutboxIntentRecord>,
    /// `OUTBOX_DISPLACED` mirror: the predecessor each open intent
    /// evicted, if any, kept durable-in-memory until `promote_signed` or
    /// `compensate_write` drops it.
    outbox_displaced: HashMap<IntentId, StoredEvent>,
    /// `OUTBOX_META`'s in-memory mirror: the next `IntentId` to allocate.
    /// The store owns this counter (never a caller) — see `IntentId`'s doc
    /// for why a caller-inferred value is unsound.
    next_intent_id: u64,
    /// The next receipt id to allocate — the identical durable-counter
    /// treatment as `next_intent_id`, for the identical reason (team-lead
    /// correction: receipts are durably retained across restart, so a
    /// caller-side receipt-id counter has `IntentId`'s exact reuse hazard).
    next_receipt_id: u64,
    /// `OUTBOX_RECEIPTS` mirror: retained receipt records, independent of
    /// `outbox_intents`'s open-work rows (architecture review correction —
    /// see `ReceiptState`'s doc). Never pruned by this unit.
    outbox_receipts: HashMap<u64, RecoveredReceipt>,
    /// Typed mirror of `OUTBOX_ATTEMPTS`, keyed by its complete stable key.
    outbox_attempts: BTreeMap<(IntentId, RelayUrl, u64), RecoveredAttempt>,
    outbox_attempt_details: BTreeMap<(IntentId, RelayUrl, u64), RecoveredAttemptDetails>,
    outbox_lanes: BTreeMap<IntentId, BTreeMap<RelayUrl, RecoveredLane>>,
    outbox_deadlines: BTreeMap<(Timestamp, IntentId, RelayUrl), LaneDeadline>,
    outbox_deadlines_by_intent: BTreeMap<IntentId, BTreeSet<(Timestamp, RelayUrl)>>,
    /// Append-only resolved route revisions, keyed by `(intent, ordinal)`.
    outbox_route_revisions: BTreeMap<(IntentId, u64), RecoveredRouteRevision>,
    /// Every still-open kind:5 intent's OWN suppression claims (see
    /// [`SuppressClaim`]'s doc) — dropped wholesale by `promote_signed`
    /// (after committing the deletion for real) or `compensate_write`
    /// (reversing it, nothing else to do).
    outbox_kind5_claims: HashMap<IntentId, Vec<SuppressClaim>>,
    /// Reverse index: which intents currently claim a given `(target id,
    /// claiming author)` pair — consulted by `is_suppressed` to decide
    /// `query` visibility. More than one intent can claim the SAME target
    /// (two independent pending deletes of the same event before either
    /// signs or cancels) — hidden while ANY claim applies, visible again
    /// only once EVERY claim on it is gone.
    suppress_by_id: HashMap<(EventId, PublicKey), HashSet<IntentId>>,
    /// Reverse index for address claims: every currently-claiming intent
    /// AND the `created_at` ceiling ITS OWN deletion staged (issue #61 P0
    /// correction) — a candidate at this address is hidden iff its OWN
    /// `created_at` is at-or-before AT LEAST ONE claimant's ceiling, not
    /// merely "some claim exists" (that would incorrectly hide a winner
    /// created AFTER every pending deletion targeting this address).
    suppress_by_addr: HashMap<AddressKey, HashMap<IntentId, Timestamp>>,
}

impl MemoryStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn get_lane(&self, key: &LaneKey) -> Option<&RecoveredLane> {
        self.outbox_lanes
            .get(&key.intent_id)
            .and_then(|lanes| lanes.get(&key.relay))
    }

    fn insert_lane(&mut self, lane: RecoveredLane) {
        self.outbox_lanes
            .entry(lane.key.intent_id)
            .or_default()
            .insert(lane.key.relay.clone(), lane);
    }

    fn lane_deadline(lane: &RecoveredLane) -> Option<LaneDeadline> {
        let (at, kind) = match lane.state {
            LaneState::Transient { eligible_at, .. } => (eligible_at, DeadlineKind::RetryEligible),
            LaneState::InFlight {
                phase: InFlightPhase::AwaitingAck { deadline },
                ..
            } => (deadline, DeadlineKind::AckTimeout),
            _ => return None,
        };
        Some(LaneDeadline {
            at,
            key: lane.key.clone(),
            lane_revision: lane.revision,
            kind,
        })
    }

    fn replace_lane(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        state: LaneState,
    ) -> Result<RecoveredLane, PersistenceError> {
        let current = self
            .get_lane(key)
            .cloned()
            .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
        if current.revision != expected_revision {
            return Err(PersistenceError("stale outbox lane revision".into()));
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?;
        if let Some(old) = Self::lane_deadline(&current) {
            self.outbox_deadlines
                .remove(&(old.at, key.intent_id, key.relay.clone()));
            if let Some(rows) = self.outbox_deadlines_by_intent.get_mut(&key.intent_id) {
                rows.remove(&(old.at, key.relay.clone()));
                if rows.is_empty() {
                    self.outbox_deadlines_by_intent.remove(&key.intent_id);
                }
            }
        }
        let lane = RecoveredLane {
            version: 1,
            key: key.clone(),
            revision,
            last_ordinal: current.last_ordinal,
            state,
        };
        if let Some(deadline) = Self::lane_deadline(&lane) {
            self.outbox_deadlines_by_intent
                .entry(key.intent_id)
                .or_default()
                .insert((deadline.at, key.relay.clone()));
            self.outbox_deadlines
                .insert((deadline.at, key.intent_id, key.relay.clone()), deadline);
        }
        self.insert_lane(lane.clone());
        Ok(lane)
    }

    /// Allocate the next `IntentId` from the store's own durable
    /// high-water mark (never inferred from the currently-open set — see
    /// `IntentId`'s doc). Starts at 1 (0 is never issued, kept free as an
    /// unambiguous "no id" sentinel for callers that want one).
    fn alloc_intent_id(&mut self) -> Result<IntentId, PersistenceError> {
        self.next_intent_id = self
            .next_intent_id
            .checked_add(1)
            .ok_or_else(|| PersistenceError("intent id exhausted".into()))?;
        Ok(IntentId(self.next_intent_id))
    }

    /// Allocate the next receipt id, same treatment as `alloc_intent_id`.
    fn alloc_receipt_id(&mut self) -> Result<u64, PersistenceError> {
        let next = self
            .next_receipt_id
            .checked_add(1)
            .ok_or_else(|| PersistenceError("receipt id exhausted".into()))?;
        if next >= (1u64 << 63) {
            return Err(PersistenceError(
                "durable receipt id namespace exhausted".into(),
            ));
        }
        self.next_receipt_id = next;
        Ok(next)
    }

    /// Write (or overwrite) one `OUTBOX_INTENTS` row plus its
    /// `OUTBOX_DISPLACED` stash, if any — `accept_write`'s journal half of
    /// the "one atomic commit" (in-memory: same call, no separate
    /// transaction to span).
    #[allow(clippy::too_many_arguments)]
    fn journal_intent(
        &mut self,
        intent_id: IntentId,
        receipt_id: u64,
        frozen: Event,
        expected_pubkey: PublicKey,
        signing_identity_ref: String,
        durability: WriteDurability,
        routing: String,
        sig_state: IntentSigState,
        accepted_at: Timestamp,
        displaced: Option<StoredEvent>,
    ) {
        self.outbox_intents.insert(
            intent_id,
            OutboxIntentRecord {
                receipt_id,
                frozen,
                expected_pubkey,
                signing_identity_ref,
                durability,
                routing,
                sig_state,
                accepted_at,
            },
        );
        if let Some(displaced) = displaced {
            self.outbox_displaced.insert(intent_id, displaced);
        }
    }

    /// Write one `OUTBOX_RECEIPTS` row at `Accepted` — `accept_write`'s
    /// receipt-retention half (architecture review correction). Always
    /// paired with `journal_intent` in the same call; never pruned by this
    /// unit.
    fn journal_receipt(
        &mut self,
        receipt_id: u64,
        intent_id: IntentId,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) {
        self.outbox_receipts.insert(
            receipt_id,
            RecoveredReceipt {
                receipt_id,
                intent_id: Some(intent_id),
                frozen_id,
                expected_pubkey,
                state: ReceiptState::Accepted,
            },
        );
    }

    /// Re-admit a durably-stashed predecessor `se` through the ordinary
    /// dedup/tombstone/supersession rules `insert` runs, preserving its
    /// FULL original provenance (both relay `seen` history and any `local`
    /// origin) rather than reconstructing it from a single fresh
    /// observation — the compensating re-insert retraction-and-negative-
    /// deltas.md §4.2 describes ("through the same one door... wins its
    /// address back by ordinary supersession rules"), never an
    /// un-supersede operation. Returns the row as it now stands if `se`
    /// actually (re)claims a slot; `None` if it is refused, deduped away,
    /// or loses the address race (`Stale` — the correct, silent §3.4
    /// outcome for a re-offered grand-predecessor: nothing churns).
    fn reinsert_stashed(&mut self, se: StoredEvent) -> Option<StoredEvent> {
        let event = se.event.clone();

        if self.by_id.contains_key(&event.id) {
            // Architecture review requirement (issue #2 P0 correction,
            // codex-nova ruling): union the owner sets and apply Signed
            // dominance — never silently drop the stashed entry's OWN
            // ownership/signature-state fact just because this exact id
            // happens to already be held. If the union newly becomes
            // Signed for previously-Pending owners, fan out to all of
            // them — the SAME invariant `promote_signed` enforces
            // explicitly, since a dedup collision here is functionally
            // no different from a relay independently confirming the
            // signature.
            let mut fan_out_owners: Option<BTreeSet<IntentId>> = None;
            {
                let existing = self
                    .by_id
                    .get_mut(&event.id)
                    .expect("just checked this id exists");
                for (relay, at) in &se.provenance.seen {
                    existing
                        .provenance
                        .merge_observation(&RelayObserved::new(relay.clone(), *at));
                }
                if let Some(stashed_local) = &se.provenance.local {
                    // codex-nova ruling (cross-door reachability finding):
                    // a row with NO local provenance at all is purely
                    // relay-observed -- its `event.sig` is by construction
                    // already real, never a sentinel -- so it counts as
                    // "already signed" exactly like a locally-owned row
                    // whose own `sig_state` is `Signed` (the SAME rule
                    // `accept_write`'s `already_signed` and `insert`'s
                    // dedup branch already apply). `unwrap_or(true)`, NOT
                    // `is_some_and` defaulting to `false` -- getting this
                    // backwards here specifically meant a relay-confirmed
                    // row restored from a stash collision never told the
                    // stash's own owner it was safe to stop waiting.
                    let existing_signed = existing
                        .provenance
                        .local
                        .as_ref()
                        .map(|l| l.sig_state == SigState::Signed)
                        .unwrap_or(true);
                    let stashed_signed = stashed_local.sig_state == SigState::Signed;
                    if !existing_signed && stashed_signed {
                        existing.event.sig = se.event.sig;
                    }
                    let mut owners = existing
                        .provenance
                        .local
                        .as_ref()
                        .map(|l| l.owners.clone())
                        .unwrap_or_default();
                    owners.extend(stashed_local.owners.iter().copied());
                    let result_signed = existing_signed || stashed_signed;
                    existing.provenance.local = Some(LocalOrigin {
                        owners: owners.clone(),
                        sig_state: if result_signed {
                            SigState::Signed
                        } else {
                            SigState::Pending
                        },
                    });
                    // Fan out whenever the RESULT is Signed, regardless of
                    // which side already held the real signature --
                    // `fan_out_signed` itself is idempotent per owner (it
                    // only transitions an owner whose OWN journal is still
                    // `Pending`), so this is always safe, and it is the
                    // ONLY way the STASH's own owner(s) ever learn that a
                    // row which was ALREADY signed on the live/relay side
                    // is done waiting on them.
                    if result_signed {
                        fan_out_owners = Some(owners);
                    }
                }
            }
            if let Some(owners) = fan_out_owners {
                let canonical = self
                    .by_id
                    .get(&event.id)
                    .expect("just updated this row")
                    .event
                    .clone();
                self.fan_out_signed(&owners, &canonical);
            }
            return Some(
                self.by_id
                    .get(&event.id)
                    .expect("just updated this row")
                    .clone(),
            );
        }
        if self.tombstone_refuses(&event) {
            return None;
        }

        match address_key_for(&event) {
            None => {
                self.index_expiration(&se);
                self.by_id.insert(event.id, se.clone());
                Some(se)
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    self.index_expiration(&se);
                    self.by_id.insert(event.id, se.clone());
                    self.addr_index.insert(key, event.id);
                    Some(se)
                }
                Some(current_id) => {
                    let current_event = &self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event")
                        .event;
                    if candidate_wins(&event, current_event) {
                        let replaced = self
                            .by_id
                            .remove(&current_id)
                            .expect("addr_index must always point at a stored event");
                        self.unindex_expiration(&replaced);
                        self.index_expiration(&se);
                        self.by_id.insert(event.id, se.clone());
                        self.addr_index.insert(key, event.id);
                        Some(se)
                    } else {
                        None
                    }
                }
            },
        }
    }

    /// Add `se` to the expiration index if it carries a NIP-40 `expiration`
    /// tag. Called for every row entering `by_id`.
    fn index_expiration(&mut self, se: &StoredEvent) {
        if let Some(ts) = se.event.tags.expiration().copied() {
            self.expiration_index
                .entry(ts)
                .or_default()
                .insert(se.event.id);
        }
    }

    /// Remove `se` from the expiration index, if it was in it. Called for
    /// every row leaving `by_id` (supersession's evicted row, `remove`).
    fn unindex_expiration(&mut self, se: &StoredEvent) {
        if let Some(ts) = se.event.tags.expiration().copied() {
            if let Some(ids) = self.expiration_index.get_mut(&ts) {
                ids.remove(&se.event.id);
                if ids.is_empty() {
                    self.expiration_index.remove(&ts);
                }
            }
        }
    }

    /// The tombstone check (retraction-and-negative-deltas.md §2): `true`
    /// iff `event` must be `Refused(Tombstoned)`. Runs for every event, not
    /// just kind:5 redeliveries — a kind:5 event's own id could itself have
    /// been the target of an earlier (unusual but not forbidden) deletion.
    ///
    /// For an id-tombstone, this is where the deferred NIP-09 author-only
    /// check happens for a target that was NOT held at deletion time (the
    /// `deleted_ids` entry was written speculatively, before this event
    /// ever arrived): refused iff `event.pubkey` itself is among the
    /// authors who have claimed this id (`deleted_ids` is keyed per-author,
    /// not collapsed to one slot -- see its doc for why). A wrong-author
    /// claim on this same id never suppresses this event: it simply isn't
    /// in the set.
    fn tombstone_refuses(&self, event: &Event) -> bool {
        if self.deleted_ids.contains_key(&(event.id, event.pubkey)) {
            return true;
        }
        if let Some(key) = address_key_for(event) {
            if let Some((ceiling, _)) = self.deleted_addrs.get(&key) {
                if event.created_at <= *ceiling {
                    return true;
                }
            }
        }
        false
    }

    /// Kind:5 processing (retraction-and-negative-deltas.md §2), run once
    /// the deleting event itself has been durably stored. For each `e`-tag
    /// id / `a`-tag coordinate: author-verify (immediately if the target is
    /// held or the coordinate carries its own pubkey; deferred via
    /// `tombstone_refuses` otherwise), write the PERMANENT tombstone, and
    /// drop the row if currently held. Returns every row actually dropped.
    fn process_kind5_deletions(&mut self, deleting: &Event) -> Vec<StoredEvent> {
        let mut deleted = Vec::new();

        let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
        for target_id in target_ids {
            let authorized_and_held = self
                .by_id
                .get(&target_id)
                .is_some_and(|se| se.event.pubkey == deleting.pubkey);
            if authorized_and_held {
                if let Some(removed) = self
                    .remove(target_id, RetractReason::Deleted)
                    .expect("MemoryStore remove is infallible")
                {
                    deleted.push(removed);
                }
            }
            // Claim recorded regardless of hold state right now -- a
            // target not yet held is checked, deferred, by
            // `tombstone_refuses` at the moment it actually arrives. NEVER
            // overwrite another author's existing claim on this same id
            // (see `deleted_ids`'s doc) -- accumulate.
            self.deleted_ids
                .insert((target_id, deleting.pubkey), deleting.id);
        }

        let coords: Vec<_> = deleting.tags.coordinates().cloned().collect();
        for coord in coords {
            if coord.public_key != deleting.pubkey {
                // NIP-09 author-only: a coordinate naming a pubkey other
                // than this deletion's own author carries no authority at
                // all here -- skip entirely, no tombstone recorded.
                continue;
            }
            let Some(key) = address_key_for_coordinate(&coord) else {
                continue;
            };

            let record = TombstoneRecord {
                deleting_event_id: deleting.id,
                deleting_author: deleting.pubkey,
            };
            let raises_ceiling = self
                .deleted_addrs
                .get(&key)
                .is_none_or(|(ceiling, _)| deleting.created_at > *ceiling);
            if raises_ceiling {
                self.deleted_addrs
                    .insert(key.clone(), (deleting.created_at, record));
            }

            if let Some(current_id) = self.addr_index.get(&key).copied() {
                let held_at_or_before = self
                    .by_id
                    .get(&current_id)
                    .is_some_and(|se| se.event.created_at <= deleting.created_at);
                if held_at_or_before {
                    if let Some(removed) = self
                        .remove(current_id, RetractReason::Deleted)
                        .expect("MemoryStore remove is infallible")
                    {
                        deleted.push(removed);
                    }
                }
            }
        }

        deleted
    }

    /// The PENDING half of kind:5 processing (architecture review
    /// requirement — see [`SuppressClaim`]'s doc): stages a REVERSIBLE
    /// suppression claim over every e-tag id target and a-tag address
    /// target this draft names, hiding whatever row currently lives there
    /// from `query` — via `is_suppressed`, consulted at read time — WITHOUT
    /// moving or removing it from `by_id`/`addr_index`. Called for EVERY
    /// accepted pending kind:5 intent, including an exact `Duplicate`
    /// (issue #61 P0 correction: a duplicate that returned before staging
    /// its own claim left it with no independent suppression, so
    /// cancelling the canonical original could reveal a target the
    /// duplicate was still obligated to delete). `promote_signed` later
    /// drops these claims and runs the FULL, permanent
    /// `process_kind5_deletions`; `compensate_write` just drops them (the
    /// target reappears immediately — nothing to re-insert, it never
    /// left). Returns the rows that ACTUALLY became newly hidden as a
    /// result of THIS call — a true visibility delta (issue #61 P1
    /// correction), computed from before/after suppression state and
    /// deduped by event id, so a target some OTHER overlapping claim
    /// already hid is never re-reported, and a target named by both an
    /// e-tag and an a-tag is never double-counted.
    fn process_kind5_deletions_provisional(
        &mut self,
        intent_id: IntentId,
        deleting: &Event,
    ) -> Vec<StoredEvent> {
        let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
        let coords: Vec<_> = deleting.tags.coordinates().cloned().collect();

        let mut candidate_ids: Vec<EventId> = Vec::new();
        let mut seen_candidates: HashSet<EventId> = HashSet::new();
        for target_id in &target_ids {
            if seen_candidates.insert(*target_id) {
                candidate_ids.push(*target_id);
            }
        }
        for coord in &coords {
            if coord.public_key != deleting.pubkey {
                continue;
            }
            if let Some(key) = address_key_for_coordinate(coord) {
                if let Some(current_id) = self.addr_index.get(&key).copied() {
                    if seen_candidates.insert(current_id) {
                        candidate_ids.push(current_id);
                    }
                }
            }
        }
        let mut visible_before: HashMap<EventId, bool> = HashMap::new();
        for id in &candidate_ids {
            let visible = self.by_id.get(id).is_some_and(|se| !self.is_suppressed(se));
            visible_before.insert(*id, visible);
        }

        let mut claims = Vec::new();
        for target_id in target_ids {
            self.suppress_by_id
                .entry((target_id, deleting.pubkey))
                .or_default()
                .insert(intent_id);
            claims.push(SuppressClaim::Id(target_id, deleting.pubkey));
        }
        for coord in coords {
            if coord.public_key != deleting.pubkey {
                // NIP-09 author-only: a coordinate naming a pubkey other
                // than this deletion's own author carries no authority at
                // all here — skip entirely, no claim staged.
                continue;
            }
            let Some(key) = address_key_for_coordinate(&coord) else {
                continue;
            };
            self.suppress_by_addr
                .entry(key.clone())
                .or_default()
                .insert(intent_id, deleting.created_at);
            claims.push(SuppressClaim::Addr(
                key,
                deleting.created_at,
                deleting.pubkey,
            ));
        }
        self.outbox_kind5_claims.insert(intent_id, claims);

        let mut hidden = Vec::new();
        for id in candidate_ids {
            if !visible_before.get(&id).copied().unwrap_or(false) {
                continue;
            }
            if let Some(se) = self.by_id.get(&id) {
                if self.is_suppressed(se) {
                    hidden.push(se.clone());
                }
            }
        }
        hidden
    }

    /// `true` iff `se` is currently hidden by ANY still-open kind:5
    /// suppression claim — consulted by `query` and `gc`. Never affects
    /// `by_id`/`addr_index` themselves: a suppressed row is fully present,
    /// just filtered out of read results (see [`SuppressClaim`]'s doc). An
    /// address claim only hides a candidate whose OWN `created_at` is
    /// at-or-before at least one claimant's ceiling (issue #61 P0
    /// correction) — mirrors the permanent `deleted_addrs` ceiling check
    /// exactly, just per-claimant instead of one shared ceiling.
    fn is_suppressed(&self, se: &StoredEvent) -> bool {
        if self
            .suppress_by_id
            .get(&(se.event.id, se.event.pubkey))
            .is_some_and(|claimants| !claimants.is_empty())
        {
            return true;
        }
        if let Some(key) = address_key_for(&se.event) {
            if let Some(claimants) = self.suppress_by_addr.get(&key) {
                if claimants
                    .values()
                    .any(|ceiling| se.event.created_at <= *ceiling)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Remove `intent_id` from every reverse-index entry `claims` named,
    /// pruning any claimant set left empty — shared by `promote_signed`
    /// (after committing the deletion) and `compensate_write` (reversing
    /// it). Never touches `by_id`/`addr_index`: a claim is pure,
    /// independently-droppable metadata.
    fn drop_kind5_claims(&mut self, intent_id: IntentId, claims: &[SuppressClaim]) {
        for claim in claims {
            match claim {
                SuppressClaim::Id(target_id, author) => {
                    if let Some(claimants) = self.suppress_by_id.get_mut(&(*target_id, *author)) {
                        claimants.remove(&intent_id);
                        if claimants.is_empty() {
                            self.suppress_by_id.remove(&(*target_id, *author));
                        }
                    }
                }
                SuppressClaim::Addr(key, _, _) => {
                    if let Some(claimants) = self.suppress_by_addr.get_mut(key) {
                        claimants.remove(&intent_id);
                        if claimants.is_empty() {
                            self.suppress_by_addr.remove(key);
                        }
                    }
                }
            }
        }
    }

    /// Atomically transition every intent in `owners` whose OWN journal is
    /// still `Pending` to `Signed`, using `canonical_event` as the frozen
    /// bytes each owner's journal now reflects, dropping each owner's own
    /// displaced stash too (R6) and closing each owner's own kind:5
    /// suppression claims if `canonical_event` is a deletion (running the
    /// FULL, permanent `process_kind5_deletions` once, not per-owner).
    /// Architecture review requirement (issue #2 P0 correction, codex-nova
    /// ruling): `promote_signed`, `reinsert_stashed`'s dedup collision,
    /// and `insert`'s relay-dedup onto a pending sentinel must all fan out
    /// IDENTICALLY — an offline co-owner signer must never strand a
    /// receipt behind an event that's already validly signed, regardless
    /// of HOW that signature became canonical. Returns every intent THIS
    /// call actually transitioned (an already-`Signed` owner is left
    /// untouched and excluded).
    fn fan_out_signed(
        &mut self,
        owners: &BTreeSet<IntentId>,
        canonical_event: &Event,
    ) -> Vec<IntentId> {
        let mut transitioned = Vec::new();
        let is_deletion = canonical_event.kind == Kind::EventDeletion;
        for owner_id in owners {
            self.outbox_displaced.remove(owner_id);
            if let Some(record) = self.outbox_intents.get_mut(owner_id) {
                if record.sig_state != IntentSigState::Signed {
                    record.sig_state = IntentSigState::Signed;
                    record.frozen = canonical_event.clone();
                    transitioned.push(*owner_id);
                }
            }
            if let Some(receipt) = self
                .outbox_receipts
                .values_mut()
                .find(|r| r.intent_id == Some(*owner_id))
            {
                receipt.state = ReceiptState::Signed;
            }
            if is_deletion {
                if let Some(claims) = self.outbox_kind5_claims.remove(owner_id) {
                    self.drop_kind5_claims(*owner_id, &claims);
                }
            }
        }
        if is_deletion {
            self.process_kind5_deletions(canonical_event);
        }
        transitioned
    }
}

/// True iff `se` is a locally-authored row still awaiting a signature —
/// the GC-exclusion predicate (Fable checkpoint R5), shared by `gc`'s
/// candidacy filter. Requires a NON-EMPTY `owners` set too (architecture
/// review correction, issue #2's ownership-set model): once every owning
/// intent has been compensated away, `local` can survive with an empty
/// `owners` set (kept standing by relay provenance or an already-signed
/// state — see `LocalOrigin`'s doc), and such a row is no longer an OPEN
/// local intent at all — it must become an ordinary GC candidate again,
/// not pinned forever for an obligation nothing still holds.
fn is_open_local_intent(se: &StoredEvent) -> bool {
    se.provenance
        .local
        .as_ref()
        .is_some_and(|l| !l.owners.is_empty() && l.sig_state == SigState::Pending)
}

impl EventStore for MemoryStore {
    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        // Refused at the door FIRST: an already-expired event is never
        // stored, so it never touches dedup or supersession at all.
        if event.is_expired_at(&from.at) {
            return Ok(InsertOutcome::Refused(RefuseReason::AlreadyExpired));
        }

        // Dedup-by-id FIRST: merge provenance, no index churn, before any
        // tombstone or supersession logic (ledger #5).
        if self.by_id.contains_key(&event.id) {
            let mut fan_out: Option<(BTreeSet<IntentId>, Event)> = None;
            let grew;
            {
                let existing = self
                    .by_id
                    .get_mut(&event.id)
                    .expect("just checked this id exists");
                grew = existing.provenance.merge_observation(&from);
                // Architecture review requirement (issue #2 P0
                // correction, codex-nova ruling): a relay delivering the
                // real signed event for a still-PENDING local draft is
                // functionally the SAME signature-adoption/fan-out
                // invariant `promote_signed` performs explicitly — adopt
                // it, mark every co-owner `Signed`, and fan out, rather
                // than silently keeping our own sentinel forever (a
                // caller-supplied `event` here is, by this door's own
                // contract, always a genuine relay delivery — never our
                // OWN sentinel — so its signature is always safe to
                // adopt).
                let needs_adoption = existing
                    .provenance
                    .local
                    .as_ref()
                    .is_some_and(|l| l.sig_state == SigState::Pending);
                if needs_adoption {
                    existing.event.sig = event.sig;
                    let local = existing
                        .provenance
                        .local
                        .as_mut()
                        .expect("just checked this row carries local provenance");
                    local.sig_state = SigState::Signed;
                    fan_out = Some((local.owners.clone(), existing.event.clone()));
                }
            }
            let satisfied_intents = if let Some((owners, canonical)) = fan_out {
                self.fan_out_signed(&owners, &canonical)
            } else {
                Vec::new()
            };
            return Ok(InsertOutcome::Duplicate {
                provenance_grew: grew,
                satisfied_intents,
            });
        }

        // Tombstone check, AFTER dedup-by-id, BEFORE storage
        // (retraction-and-negative-deltas.md §2).
        if self.tombstone_refuses(&event) {
            return Ok(InsertOutcome::Refused(RefuseReason::Tombstoned));
        }

        let is_deletion = event.kind == Kind::EventDeletion;
        let stored = StoredEvent {
            event: event.clone(),
            provenance: Provenance::first_observation(from),
        };

        let outcome = match address_key_for(&event) {
            None => {
                // Regular event: no competition, always inserted.
                self.index_expiration(&stored);
                self.by_id.insert(event.id, stored);
                InsertOutcome::Inserted
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    // First event ever seen at this address.
                    let id = event.id;
                    self.index_expiration(&stored);
                    self.by_id.insert(id, stored);
                    self.addr_index.insert(key, id);
                    InsertOutcome::Inserted
                }
                Some(current_id) => {
                    let current_event = &self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event")
                        .event;

                    if candidate_wins(&event, current_event) {
                        let new_id = event.id;
                        let replaced = self
                            .by_id
                            .remove(&current_id)
                            .expect("addr_index must always point at a stored event");
                        self.unindex_expiration(&replaced);
                        self.index_expiration(&stored);
                        self.by_id.insert(new_id, stored);
                        self.addr_index.insert(key, new_id);
                        InsertOutcome::Superseded {
                            replaced: Box::new(replaced),
                        }
                    } else {
                        // Older-for-existing-address: rejected, dropped.
                        InsertOutcome::Stale
                    }
                }
            },
        };

        // Kind:5 has no replaceable/addressable address (M1's set excludes
        // it), so `outcome` above is always `Inserted` here, by
        // construction -- process its deletions now that the event itself
        // is durably stored (re-servable, per §2).
        if is_deletion {
            if let InsertOutcome::Inserted = outcome {
                let deleted = self.process_kind5_deletions(&event);
                return Ok(InsertOutcome::Kind5Processed { deleted });
            }
        }

        Ok(outcome)
    }

    fn remove(
        &mut self,
        id: EventId,
        _reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        let Some(removed) = self.by_id.remove(&id) else {
            return Ok(None);
        };
        // Clear the address index too, but ONLY if it still points at the
        // row we just removed — `id` may be a non-addressed regular event
        // (no entry to clear), or a stale/superseded id that never held the
        // address slot in the first place.
        if let Some(key) = address_key_for(&removed.event) {
            if self.addr_index.get(&key) == Some(&id) {
                self.addr_index.remove(&key);
            }
        }
        self.unindex_expiration(&removed);
        Ok(Some(removed))
    }

    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        let due: Vec<EventId> = self
            .expiration_index
            .range(..=now)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();

        // `remove` is infallible for `MemoryStore`; unwrap the never-`Err`
        // `Result` here so the drain keeps its `filter_map` shape.
        Ok(due
            .into_iter()
            .filter_map(|id| {
                self.remove(id, RetractReason::Expired)
                    .expect("MemoryStore remove is infallible")
            })
            .collect())
    }

    fn next_expiration(&self) -> Option<Timestamp> {
        self.expiration_index.keys().next().copied()
    }

    fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        // `by_id` holds exactly the current winners (regular events, plus
        // the one live event per replaceable/addressable address) — so
        // iterating it and matching is "current winners only" by
        // construction. Matching is delegated entirely to
        // `nostr::Filter::match_event`; no hand-rolled matching here.
        // `is_suppressed` additionally excludes anything a still-open
        // kind:5 intent has provisionally claimed (architecture review
        // requirement — see `SuppressClaim`'s doc): the row stays fully
        // present in `by_id`, only hidden from this read path.
        Ok(self
            .by_id
            .values()
            .filter(|se| !self.is_suppressed(se))
            .filter(|se| filter.match_event(&se.event, MatchEventOptions::new()))
            .cloned()
            .collect())
    }

    fn record_coverage(
        &mut self,
        atom: &ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        let key = coverage_key(atom);
        let shape = window_erase(&atom.filter);
        let entry_key = (key, relay.clone());
        let existing = self.coverage.get(&entry_key).map(|row| row.interval);
        let merged = merge_interval(existing, proven);
        self.coverage.insert(
            entry_key,
            CoverageRow {
                shape,
                interval: merged,
            },
        );
        Ok(())
    }

    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.coverage
            .get(&(key, relay.clone()))
            .map(|row| row.interval)
    }

    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        let mut report = GcReport::default();

        // Regular events (no address key) matched by no live claim, AND not
        // an open (unsigned) local intent, are the ONLY GC candidates:
        // replaceable/addressable current winners are never in this set at
        // all (retained unconditionally, by construction), and neither is
        // an unsigned pending row (Fable checkpoint R5 — an open intent
        // must never be evicted before it ever signs; once
        // `promote_signed` flips it to `Signed` it becomes an ordinary
        // event again, GC-able like any other under `claims`). A row
        // currently hidden by a still-open kind:5 suppression claim is
        // pinned the same way (architecture review requirement — GC must
        // never evict a target a pending cancel/promote can still act on;
        // NIP-40 expiry may still remove it, that's a separate, accepted
        // path).
        let victims: Vec<EventId> = self
            .by_id
            .iter()
            .filter(|(_, se)| {
                address_key_for(&se.event).is_none()
                    && !is_open_local_intent(se)
                    && !self.is_suppressed(se)
                    && !claims.is_claimed(&se.event)
            })
            .map(|(id, _)| *id)
            .collect();

        for id in victims {
            let se = self
                .by_id
                .remove(&id)
                .expect("victim id was just found in by_id");
            report.events_evicted += 1;
            let evicted_at = se.event.created_at;

            let mut to_delete = Vec::new();
            for (row_key, row) in self.coverage.iter_mut() {
                if row.interval.from <= evicted_at
                    && evicted_at <= row.interval.through
                    && shape_matches(&row.shape, &se.event)
                {
                    match shrink_after_eviction(row.interval, evicted_at) {
                        Some(shrunk) => {
                            row.interval = shrunk;
                            report.coverage_rows_shrunk += 1;
                        }
                        None => to_delete.push(row_key.clone()),
                    }
                }
            }
            for row_key in to_delete {
                self.coverage.remove(&row_key);
                report.coverage_rows_deleted += 1;
            }
        }

        Ok(report)
    }

    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        let AcceptWrite {
            frozen,
            expected_pubkey,
            signing_identity_ref,
            durability,
            routing,
            sig_state,
            accepted_at,
        } = accept;

        // Refused at the door FIRST, same as `insert`: never journaled,
        // nothing to recover, and (R7 correction) neither an `IntentId`
        // nor a receipt id is ever allocated for a refused call — a
        // refusal can never burn either.
        if frozen.is_expired_at(&accepted_at) {
            return Ok(AcceptOutcome::Refused(RefuseReason::AlreadyExpired));
        }
        if self.tombstone_refuses(&frozen) {
            return Ok(AcceptOutcome::Refused(RefuseReason::Tombstoned));
        }

        let intent_id = self.alloc_intent_id()?;
        let receipt_id = self.alloc_receipt_id()?;
        let local = LocalOrigin {
            owners: BTreeSet::from([intent_id]),
            sig_state: SigState::Pending,
        };

        // Dedup-by-id: an edge case (a fresh intent's frozen id colliding
        // with an already-held row), NOT the ordinary relay-echo hand-off
        // (that always arrives through `insert`, after this row's real
        // signature already replaced the sentinel — see `promote_signed`'s
        // doc). The intent is still journaled: it still gets signed and
        // delivered even though it does not WIN a fresh row here. Checked
        // against BOTH the live `EVENTS` row AND every OTHER intent's
        // `OUTBOX_DISPLACED` stash (issue #2 P0 correction, codex-nova
        // ruling): a duplicate accepted while its canonical predecessor
        // is currently sitting displaced (superseded by a later local
        // edit, not yet restored) must ALSO join that stash entry's owner
        // set — otherwise it would be silently treated as a fresh insert,
        // stranding it outside the shared ownership entirely.
        enum DupLoc {
            Live,
            Stash(IntentId),
        }
        let dup_loc = if self.by_id.contains_key(&frozen.id) {
            Some(DupLoc::Live)
        } else {
            self.outbox_displaced
                .iter()
                .find(|(_, se)| se.event.id == frozen.id)
                .map(|(k, _)| DupLoc::Stash(*k))
        };
        if let Some(dup_loc) = dup_loc {
            let frozen_id = frozen.id;
            let existing = match dup_loc {
                DupLoc::Live => self
                    .by_id
                    .get(&frozen_id)
                    .expect("just checked this id exists")
                    .clone(),
                DupLoc::Stash(stash_key) => self
                    .outbox_displaced
                    .get(&stash_key)
                    .expect("just found this key")
                    .clone(),
            };
            // codex-nova ruling: a row with NO local provenance at all is
            // purely relay-observed — its `event.sig` is by construction
            // already real (never a sentinel, since `insert` only ever
            // stores what a relay actually delivered), so it counts as
            // "already signed" exactly like a locally-owned row whose own
            // `sig_state` is `Signed`.
            let already_signed = existing
                .provenance
                .local
                .as_ref()
                .map(|l| l.sig_state == SigState::Signed)
                .unwrap_or(true);

            // Architecture review correction (issue #2, team-lead
            // decision): this new intent joins the existing row's owner
            // set — an exact `Duplicate` must retain INDEPENDENT ownership
            // rather than being silently coalesced into whichever intent
            // already backs the row (see `LocalOrigin`'s doc for why
            // coalescing was rejected). This now applies even to a
            // PURELY relay-observed row (codex-nova ruling): its
            // `local` becomes `Some` for the first time, tracking this
            // intent's own obligation.
            let mut owners = existing
                .provenance
                .local
                .as_ref()
                .map(|l| l.owners.clone())
                .unwrap_or_default();
            owners.insert(intent_id);
            let row_sig_state = existing
                .provenance
                .local
                .as_ref()
                .map(|l| l.sig_state)
                .unwrap_or(SigState::Signed);
            let updated_local = Some(LocalOrigin {
                owners,
                sig_state: row_sig_state,
            });
            match dup_loc {
                DupLoc::Live => {
                    self.by_id
                        .get_mut(&frozen_id)
                        .expect("just checked this id exists")
                        .provenance
                        .local = updated_local;
                }
                DupLoc::Stash(stash_key) => {
                    self.outbox_displaced
                        .get_mut(&stash_key)
                        .expect("just found this key")
                        .provenance
                        .local = updated_local;
                }
            }

            // Issue #61 P0 correction: an exact-duplicate kind:5 intent
            // must own an INDEPENDENT suppression claim too — otherwise
            // cancelling the canonical original while this duplicate
            // remains pending would incorrectly reveal a target it is
            // still obligated to delete (see `process_kind5_deletions_
            // provisional`'s doc). Only meaningful while still PENDING —
            // an already-signed kind:5's tombstones are already permanent,
            // nothing provisional left to claim.
            if frozen.kind == Kind::EventDeletion && !already_signed {
                self.process_kind5_deletions_provisional(intent_id, &frozen);
            }
            let row = match dup_loc {
                DupLoc::Live => self
                    .by_id
                    .get(&frozen_id)
                    .expect("just checked this id exists")
                    .clone(),
                DupLoc::Stash(stash_key) => self
                    .outbox_displaced
                    .get(&stash_key)
                    .expect("just found this key")
                    .clone(),
            };

            // codex-nova ruling: a duplicate of an ALREADY-signed row
            // (local or relay) must itself start `Signed`, journaling the
            // CANONICAL bytes (`row.event`, not this call's own
            // sentinel-signed `frozen`) — an offline co-owner signer must
            // never strand a receipt behind an event that's already
            // validly signed, and there is nothing left for THIS intent
            // to sign.
            let (journaled_frozen, journaled_sig_state) = if already_signed {
                (row.event.clone(), IntentSigState::Signed)
            } else {
                (frozen, sig_state)
            };
            self.journal_intent(
                intent_id,
                receipt_id,
                journaled_frozen,
                expected_pubkey,
                signing_identity_ref,
                durability,
                routing,
                journaled_sig_state,
                accepted_at,
                None,
            );
            self.journal_receipt(receipt_id, intent_id, frozen_id, expected_pubkey);
            if already_signed {
                if let Some(receipt) = self.outbox_receipts.get_mut(&receipt_id) {
                    receipt.state = ReceiptState::Signed;
                }
            }
            return Ok(AcceptOutcome::Duplicate {
                intent_id,
                receipt_id,
                row,
            });
        }

        let stored = StoredEvent {
            event: frozen.clone(),
            provenance: Provenance::local_origin(local),
        };
        let is_deletion = frozen.kind == Kind::EventDeletion;

        let (outcome, displaced) = match address_key_for(&stored.event) {
            None => {
                self.index_expiration(&stored);
                self.by_id.insert(stored.event.id, stored.clone());
                // Architecture review correction: a locally-composed
                // kind:5 draft stages a REVERSIBLE suppression claim over
                // every target it names, immediately, in this same call —
                // issue #2's "no app optimistic mirror" promise extends to
                // local deletions too (kind:5 has no replaceable/
                // addressable address, so this branch is the only one it
                // can ever reach, mirroring `insert`'s own kind:5
                // invariant). See `SuppressClaim`'s doc for why this
                // hides rather than removes: `compensate_write` can then
                // simply drop the claim (nothing to re-insert, the row
                // never left), and the target's OWN promote_signed/
                // compensate_write keep working on exactly the row they
                // always did.
                if is_deletion {
                    let hidden = self.process_kind5_deletions_provisional(intent_id, &frozen);
                    (
                        AcceptOutcome::Kind5Processed {
                            intent_id,
                            receipt_id,
                            row: stored,
                            hidden,
                        },
                        None,
                    )
                } else {
                    (
                        AcceptOutcome::Inserted {
                            intent_id,
                            receipt_id,
                            row: stored,
                        },
                        None,
                    )
                }
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    let id = stored.event.id;
                    self.index_expiration(&stored);
                    self.by_id.insert(id, stored.clone());
                    self.addr_index.insert(key, id);
                    (
                        AcceptOutcome::Inserted {
                            intent_id,
                            receipt_id,
                            row: stored,
                        },
                        None,
                    )
                }
                Some(current_id) => {
                    let current_event = &self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event")
                        .event;

                    if candidate_wins(&stored.event, current_event) {
                        let new_id = stored.event.id;
                        let replaced = self
                            .by_id
                            .remove(&current_id)
                            .expect("addr_index must always point at a stored event");
                        self.unindex_expiration(&replaced);
                        self.index_expiration(&stored);
                        self.by_id.insert(new_id, stored.clone());
                        self.addr_index.insert(key, new_id);
                        (
                            AcceptOutcome::Superseded {
                                intent_id,
                                receipt_id,
                                row: stored,
                                replaced: Box::new(replaced.clone()),
                            },
                            Some(replaced),
                        )
                    } else {
                        (
                            AcceptOutcome::Stale {
                                intent_id,
                                receipt_id,
                            },
                            None,
                        )
                    }
                }
            },
        };

        let frozen_id = frozen.id;
        self.journal_intent(
            intent_id,
            receipt_id,
            frozen,
            expected_pubkey,
            signing_identity_ref,
            durability,
            routing,
            sig_state,
            accepted_at,
            displaced,
        );
        self.journal_receipt(receipt_id, intent_id, frozen_id, expected_pubkey);

        Ok(outcome)
    }

    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        sig: Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        let Some(intent_record) = self.outbox_intents.get(&intent_id) else {
            return Ok(PromoteOutcome::NotFound);
        };
        // No-second-transition guard (codex-nova finding): a repeat
        // promotion (e.g. a duplicate signer completion) must not
        // overwrite an already-Signed row and re-emit `Promoted` — the
        // trait doc already promised "already-promoted returns NotFound";
        // this enforces it. Load-bearing for `AtMostOnce`: a second
        // silent transition here could let the caller re-publish.
        if intent_record.sig_state == IntentSigState::Signed {
            return Ok(PromoteOutcome::NotFound);
        }
        let frozen_id = intent_record.frozen.id;

        // Architecture review correction (load-bearing): is this intent
        // AMONG the owners of the LIVE row at its own frozen id? A
        // `Duplicate`/`Stale` intent never had one of its own; a once-live
        // row can since have been superseded (locally or by a relay),
        // kind:5-deleted, or expired. Ownership is a SET (issue #2,
        // team-lead decision): an exact `Duplicate` is a CO-OWNER of the
        // SAME canonical row, not a second row of its own — see
        // `LocalOrigin`'s doc.
        let live = self.by_id.get(&frozen_id).is_some_and(|se| {
            se.provenance
                .local
                .as_ref()
                .is_some_and(|l| l.owners.contains(&intent_id))
        });

        // Row-level already-signed check: is the shared row/stash entry
        // ALREADY signed by some OTHER co-owner? Structurally this should
        // never actually be reached in a healthy run any more (see below)
        // — the eager cross-owner propagation this call itself performs
        // means the per-intent guard above already catches a co-owner's
        // OWN later call — but it is kept as a defensive fallback: never
        // overwrite a canonical signature that's already there.
        let already_signed = if live {
            self.by_id
                .get(&frozen_id)
                .and_then(|se| se.provenance.local.as_ref())
                .is_some_and(|l| l.sig_state == SigState::Signed)
        } else {
            self.outbox_displaced.values().any(|se| {
                se.event.id == frozen_id
                    && se.provenance.local.as_ref().is_some_and(|l| {
                        l.owners.contains(&intent_id) && l.sig_state == SigState::Signed
                    })
            })
        };

        let (row, owners) = if live {
            let se = self
                .by_id
                .get_mut(&frozen_id)
                .expect("just checked this row is live for this intent");
            if !already_signed {
                se.event.sig = sig;
                se.provenance
                    .local
                    .as_mut()
                    .expect("just checked this row carries local provenance")
                    .sig_state = SigState::Signed;
            }
            let owners = se
                .provenance
                .local
                .as_ref()
                .expect("just checked this row carries local provenance")
                .owners
                .clone();
            (se.clone(), owners)
        } else if let Some(other) = self.outbox_displaced.values_mut().find(|se| {
            // Not live. If this intent's exact frozen bytes are sitting in
            // some OTHER intent's displaced stash (it was superseded by a
            // later local edit before it could sign), sync the real
            // signature there too — otherwise a future restore of that
            // stash entry would resurrect a stale sentinel copy of an
            // intent that actually did sign. Matched by OWNING intent_id
            // membership, NOT bare event id (codex-nova finding): two
            // different intents (e.g. a real one and its byte-identical
            // `Duplicate`) can share the same frozen event id, and only a
            // stash entry whose OWN `LocalOrigin::owners` set CONTAINS
            // `intent_id` may ever be touched here.
            se.event.id == frozen_id
                && se
                    .provenance
                    .local
                    .as_ref()
                    .is_some_and(|l| l.owners.contains(&intent_id))
        }) {
            if !already_signed {
                other.event.sig = sig;
                if let Some(local) = other.provenance.local.as_mut() {
                    local.sig_state = SigState::Signed;
                }
            }
            let owners = other
                .provenance
                .local
                .as_ref()
                .expect("just matched an owned stash entry")
                .owners
                .clone();
            (other.clone(), owners)
        } else {
            // Neither live nor in anyone's stash — synthesize the
            // resulting signed bytes from the journal's own copy. The
            // engine can still publish these even though this intent does
            // not (or no longer) win any local address. Only reachable
            // when `!already_signed`: `already_signed` requires a matching
            // live row or stash entry to have been found above.
            let mut event = self
                .outbox_intents
                .get(&intent_id)
                .expect("looked up at the top of this call")
                .frozen
                .clone();
            event.sig = sig;
            (
                StoredEvent {
                    event,
                    provenance: Provenance {
                        seen: BTreeMap::new(),
                        local: Some(LocalOrigin {
                            owners: BTreeSet::from([intent_id]),
                            sig_state: SigState::Signed,
                        }),
                    },
                },
                BTreeSet::from([intent_id]),
            )
        };

        // codex-nova ruling (tightened after review): the FIRST owner to
        // sign atomically transitions EVERY co-owner's OWN journal/
        // receipt to `Signed` against the SAME canonical bytes, in THIS
        // SAME call — never lazily deferred until (or unless) each
        // co-owner separately calls `promote_signed` itself. `co_signed`
        // excludes `intent_id` itself (already conveyed by `Promoted`'s
        // own `row`).
        let co_signed = self
            .fan_out_signed(&owners, &row.event)
            .into_iter()
            .filter(|owner_id| *owner_id != intent_id)
            .collect();

        Ok(PromoteOutcome::Promoted {
            row: Box::new(row),
            co_signed,
        })
    }

    fn compensate_write(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        let Some(intent_record) = self.outbox_intents.get(&intent_id) else {
            return Ok(CompensateOutcome::NotFound);
        };
        // Pre-signature only (retraction doc §4.2's "Promotion
        // correction"): once `promote_signed` has run, this door refuses.
        if intent_record.sig_state == IntentSigState::Signed {
            return Ok(CompensateOutcome::NotFound);
        }
        let frozen_id = intent_record.frozen.id;

        let live = self.by_id.get(&frozen_id).is_some_and(|se| {
            se.provenance
                .local
                .as_ref()
                .is_some_and(|l| l.owners.contains(&intent_id))
        });

        if live {
            // Architecture review correction (issue #2, team-lead
            // decision): removing THIS intent from the row's owner set
            // only actually retracts the canonical row once the set is
            // EMPTY, `sig_state` is still `Pending`, AND no relay has
            // independently confirmed it — an exact-`Duplicate`'s still-
            // open obligation, an already-`Signed` state some OTHER owner
            // committed, or independent relay provenance, must all
            // survive this one intent's cancellation (see `LocalOrigin`'s
            // doc). §4.2: `remove(id, Rejected)` writes no tombstone
            // (`remove` never writes one — only kind:5 processing does).
            let se = self
                .by_id
                .get_mut(&frozen_id)
                .expect("just checked this row is live for this intent");
            let local = se
                .provenance
                .local
                .as_mut()
                .expect("just checked this row carries local provenance");
            local.owners.remove(&intent_id);
            let should_retract = local.owners.is_empty()
                && local.sig_state == SigState::Pending
                && se.provenance.seen.is_empty();
            if should_retract {
                self.remove(frozen_id, RetractReason::Rejected)
                    .expect("MemoryStore remove is infallible");
            }
        } else {
            // Not live. If sitting in someone else's displaced stash
            // (chained local supersession before this intent could sign),
            // remove THIS intent from THAT stash entry's owner set, same
            // conditional-retraction rule as the live case above — an
            // exact-`Duplicate` co-owner (or a signed/relay-confirmed
            // state) sitting in the SAME stash slot must survive this
            // intent's cancellation too. Matched by OWNING intent_id
            // SET-membership, not bare event id — see `promote_signed`'s
            // identical fix for why (a `Duplicate` can share an event id
            // with an unrelated, real intent).
            let other_key = self
                .outbox_displaced
                .iter()
                .find(|(_, se)| {
                    se.event.id == frozen_id
                        && se
                            .provenance
                            .local
                            .as_ref()
                            .is_some_and(|l| l.owners.contains(&intent_id))
                })
                .map(|(k, _)| *k);
            if let Some(other_key) = other_key {
                let se = self
                    .outbox_displaced
                    .get_mut(&other_key)
                    .expect("just found this key");
                let local = se
                    .provenance
                    .local
                    .as_mut()
                    .expect("just checked this entry carries local provenance");
                local.owners.remove(&intent_id);
                let should_drop = local.owners.is_empty()
                    && local.sig_state == SigState::Pending
                    && se.provenance.seen.is_empty();
                if should_drop {
                    self.outbox_displaced.remove(&other_key);
                }
            }
        }

        self.outbox_intents.remove(&intent_id);
        // THIS intent's OWN displaced predecessor (if any) is restored
        // through the same one door regardless of whether its row was
        // live or already gone for some other reason (kind:5/expiry/relay
        // supersession) — `reinsert_stashed`'s own tombstone check makes
        // this safe even if the predecessor was itself since deleted or
        // expired.
        let restored = self
            .outbox_displaced
            .remove(&intent_id)
            .and_then(|displaced| self.reinsert_stashed(displaced))
            .map(Box::new);

        // Architecture review requirement (kind:5 suppression-claim
        // reversal): if this was a still-pending kind:5 draft, drop its
        // OWN claims outright — nothing was ever moved or removed, so
        // there is nothing to re-insert. `revealed` is a true visibility
        // DELTA (issue #61 P1 correction), computed from before/after
        // suppression state and deduped by event id — so a target still
        // hidden by some OTHER intent's overlapping claim, one already
        // gone for good because a different intent already promoted its
        // own deletion of the same target, or one this claim's own
        // (author/ceiling) component never actually covered in the first
        // place (e.g. a wrong-author e-tag claim on a row some OTHER
        // author holds), is correctly excluded.
        let mut revealed = Vec::new();
        if let Some(claims) = self.outbox_kind5_claims.remove(&intent_id) {
            let mut candidate_ids: Vec<EventId> = Vec::new();
            let mut seen_candidates: HashSet<EventId> = HashSet::new();
            for claim in &claims {
                let target_id = match claim {
                    SuppressClaim::Id(target_id, _) => Some(*target_id),
                    SuppressClaim::Addr(key, _, _) => self.addr_index.get(key).copied(),
                };
                if let Some(target_id) = target_id {
                    if seen_candidates.insert(target_id) {
                        candidate_ids.push(target_id);
                    }
                }
            }
            let mut visible_before: HashMap<EventId, bool> = HashMap::new();
            for id in &candidate_ids {
                let visible = self.by_id.get(id).is_some_and(|se| !self.is_suppressed(se));
                visible_before.insert(*id, visible);
            }

            self.drop_kind5_claims(intent_id, &claims);

            for id in candidate_ids {
                if visible_before.get(&id).copied().unwrap_or(false) {
                    continue;
                }
                if let Some(se) = self.by_id.get(&id) {
                    if !self.is_suppressed(se) {
                        revealed.push(se.clone());
                    }
                }
            }
        }

        if let Some(receipt) = self
            .outbox_receipts
            .values_mut()
            .find(|r| r.intent_id == Some(intent_id))
        {
            receipt.state = ReceiptState::Compensated;
        }

        Ok(CompensateOutcome::Compensated { restored, revealed })
    }

    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        // Fable checkpoint Q4: crash-safety is a `RedbStore`-only backend
        // property. Nothing here survives a real process crash, so there
        // is nothing to recover, by construction.
        Vec::new()
    }

    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        // NOT a Q4 "always empty" door: retention (not crash-survival) is
        // the contract here, and `MemoryStore` retains faithfully for the
        // life of the process — see `EventStore::reattach_receipt`'s doc.
        Ok(self.outbox_receipts.get(&receipt_id).cloned())
    }

    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        if !self.outbox_intents.contains_key(&intent_id) {
            return Err(PersistenceError("route revision intent is not open".into()));
        }
        let ordinal = self
            .outbox_route_revisions
            .keys()
            .filter(|(candidate, _)| *candidate == intent_id)
            .map(|(_, ordinal)| *ordinal)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| PersistenceError("route revision ordinal exhausted".into()))?;
        let revision = RecoveredRouteRevision {
            version: 1,
            intent_id,
            ordinal,
            relays,
        };
        self.outbox_route_revisions
            .insert((intent_id, ordinal), revision.clone());
        Ok(revision)
    }

    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        Ok(self
            .outbox_route_revisions
            .values()
            .filter(|revision| revision.intent_id == intent_id)
            .cloned()
            .collect())
    }

    fn start_attempt(
        &mut self,
        intent_id: IntentId,
        relay: RelayUrl,
        event: Event,
    ) -> Result<RecoveredAttempt, PersistenceError> {
        let Some(intent) = self.outbox_intents.get(&intent_id) else {
            return Err(PersistenceError("attempt intent is not open".into()));
        };
        if intent.sig_state != IntentSigState::Signed || intent.frozen != event {
            return Err(PersistenceError(
                "attempt bytes are not the intent's promoted signed bytes".into(),
            ));
        }
        event
            .verify()
            .map_err(|err| PersistenceError(format!("attempt event is invalid: {err}")))?;
        let lane_key = LaneKey {
            intent_id,
            relay: relay.clone(),
        };
        let current = self.get_lane(&lane_key).cloned().unwrap_or_else(|| {
            let lane = RecoveredLane {
                version: 1,
                key: lane_key.clone(),
                revision: 1,
                last_ordinal: 0,
                state: LaneState::WaitingConnection,
            };
            self.insert_lane(lane.clone());
            lane
        });
        if current.last_ordinal > 0 {
            let previous_key = (intent_id, relay.clone(), current.last_ordinal);
            let previous = self
                .outbox_attempts
                .get(&previous_key)
                .ok_or_else(|| PersistenceError("lane cursor attempt row not found".into()))?;
            if crate::attempt_is_live(previous, self.outbox_attempt_details.get(&previous_key)) {
                return Err(PersistenceError(
                    "cannot start a new ordinal while the current attempt is live".into(),
                ));
            }
        }
        current
            .revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?;
        let ordinal = current
            .last_ordinal
            .checked_add(1)
            .ok_or_else(|| PersistenceError("attempt ordinal exhausted".into()))?;
        let attempt = RecoveredAttempt {
            version: 1,
            intent_id,
            relay: relay.clone(),
            ordinal,
            event,
            outcome: AttemptOutcome::Started,
        };
        self.outbox_attempts
            .insert((intent_id, relay.clone(), ordinal), attempt.clone());
        self.outbox_attempt_details.insert(
            (intent_id, relay.clone(), ordinal),
            RecoveredAttemptDetails {
                version: 1,
                intent_id,
                relay: relay.clone(),
                ordinal,
                started_at: None,
                handoff: None,
                transient: None,
                finished_at: None,
                terminal: None,
            },
        );
        let mut lane = self.replace_lane(
            &lane_key,
            current.revision,
            LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingHandoff,
            },
        )?;
        lane.last_ordinal = ordinal;
        self.insert_lane(lane);
        Ok(attempt)
    }

    fn finish_attempt(
        &mut self,
        intent_id: IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError> {
        if outcome == AttemptOutcome::Started {
            return Err(PersistenceError("Started is not a terminal outcome".into()));
        }
        let Some(attempt) = self
            .outbox_attempts
            .get(&(intent_id, relay.clone(), ordinal))
        else {
            return Err(PersistenceError("attempt row not found".into()));
        };
        if attempt.outcome != AttemptOutcome::Started {
            if attempt.outcome == outcome {
                return Ok(FinishAttemptOutcome::AlreadySame);
            }
            return Err(PersistenceError(
                "legacy attempt row has a conflicting terminal outcome".into(),
            ));
        }
        let key = LaneKey {
            intent_id,
            relay: relay.clone(),
        };
        let lane = self
            .get_lane(&key)
            .cloned()
            .ok_or_else(|| PersistenceError("attempt lane not found".into()))?;
        if lane.last_ordinal != ordinal {
            return Err(PersistenceError("stale attempt ordinal".into()));
        }
        lane.revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?;
        let details = self
            .outbox_attempt_details
            .get_mut(&(intent_id, relay.clone(), ordinal))
            .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
        if details.terminal.as_ref() == Some(&outcome) {
            return Ok(FinishAttemptOutcome::AlreadySame);
        }
        if details.terminal.is_some() {
            return Err(PersistenceError(
                "attempt already has a conflicting terminal outcome".into(),
            ));
        }
        details.terminal = Some(outcome.clone());
        self.replace_lane(
            &key,
            lane.revision,
            LaneState::Terminal { ordinal, outcome },
        )?;
        Ok(FinishAttemptOutcome::Committed)
    }

    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        let mut recovered: Vec<_> = self
            .outbox_attempts
            .values()
            .filter(|attempt| attempt.intent_id == intent_id)
            .map(|attempt| {
                let mut effective = attempt.clone();
                if let Some(terminal) = self
                    .outbox_attempt_details
                    .get(&(attempt.intent_id, attempt.relay.clone(), attempt.ordinal))
                    .and_then(|details| details.terminal.clone())
                {
                    effective.outcome = terminal;
                }
                effective
            })
            .collect();
        recovered.sort_by(|left, right| {
            left.relay
                .cmp(&right.relay)
                .then(left.ordinal.cmp(&right.ordinal))
        });
        Ok(recovered)
    }

    fn bootstrap_outbox_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        if !self.outbox_intents.contains_key(&intent_id) {
            return Err(PersistenceError("lane bootstrap intent is not open".into()));
        }
        let mut relays = BTreeSet::new();
        for revision in self.outbox_route_revisions.values() {
            if revision.intent_id == intent_id {
                relays.extend(revision.relays.iter().cloned());
            }
        }
        for (candidate, relay, _) in self.outbox_attempts.keys() {
            if *candidate == intent_id {
                relays.insert(relay.clone());
            }
        }
        let all_attempts = self.recover_attempts(intent_id)?;
        for relay in relays {
            let key = LaneKey { intent_id, relay };
            let attempts: Vec<_> = all_attempts
                .iter()
                .filter(|attempt| attempt.relay == key.relay)
                .cloned()
                .collect();
            let live_count = attempts
                .iter()
                .filter(|attempt| {
                    crate::attempt_is_live(
                        attempt,
                        self.outbox_attempt_details.get(&(
                            attempt.intent_id,
                            attempt.relay.clone(),
                            attempt.ordinal,
                        )),
                    )
                })
                .count();
            if live_count > 1
                || (live_count == 1
                    && attempts.last().is_some_and(|attempt| {
                        !crate::attempt_is_live(
                            attempt,
                            self.outbox_attempt_details.get(&(
                                attempt.intent_id,
                                attempt.relay.clone(),
                                attempt.ordinal,
                            )),
                        )
                    }))
            {
                return Err(PersistenceError(
                    "contradictory live v1 Started attempt history".into(),
                ));
            }
            if let Some(existing) = self.get_lane(&key) {
                let max = attempts.last().map_or(0, |attempt| attempt.ordinal);
                if existing.last_ordinal != max {
                    return Err(PersistenceError(
                        "outbox lane cursor disagrees with retained attempt history".into(),
                    ));
                }
                match attempts.last() {
                    Some(attempt) if attempt.outcome != AttemptOutcome::Started => {
                        if existing.state
                            != (LaneState::Terminal {
                                ordinal: attempt.ordinal,
                                outcome: attempt.outcome.clone(),
                            })
                        {
                            return Err(PersistenceError(
                                "terminal attempt and lane state disagree".into(),
                            ));
                        }
                    }
                    _ if matches!(existing.state, LaneState::Terminal { .. }) => {
                        return Err(PersistenceError(
                            "terminal lane lacks matching terminal attempt".into(),
                        ));
                    }
                    _ => {}
                }
                continue;
            }
            let last_ordinal = attempts.last().map_or(0, |attempt| attempt.ordinal);
            let state = match attempts.last() {
                None => LaneState::WaitingConnection,
                Some(attempt) if attempt.outcome == AttemptOutcome::Started => {
                    LaneState::LegacyInFlight {
                        ordinal: attempt.ordinal,
                    }
                }
                Some(attempt) => LaneState::Terminal {
                    ordinal: attempt.ordinal,
                    outcome: attempt.outcome.clone(),
                },
            };
            self.insert_lane(RecoveredLane {
                version: 1,
                key,
                revision: 1,
                last_ordinal,
                state,
            });
        }
        self.recover_outbox_lanes(intent_id)
    }

    fn recover_outbox_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        Ok(self
            .outbox_lanes
            .get(&intent_id)
            .into_iter()
            .flat_map(|lanes| lanes.values().cloned())
            .collect())
    }

    fn due_outbox_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<LaneDeadline>, PersistenceError> {
        if limit > 1_024 {
            return Err(PersistenceError("deadline read limit exceeds 1024".into()));
        }
        let mut due = Vec::new();
        for (_, deadline) in self.outbox_deadlines.range(
            ..=(
                now,
                IntentId(u64::MAX),
                RelayUrl::parse("wss://zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
                    .expect("static relay"),
            ),
        ) {
            if due.len() == limit {
                break;
            }
            let lane = self
                .get_lane(&deadline.key)
                .ok_or_else(|| PersistenceError("deadline references missing lane".into()))?;
            if Self::lane_deadline(lane).as_ref() != Some(deadline) {
                return Err(PersistenceError("deadline and lane disagree".into()));
            }
            due.push(deadline.clone());
        }
        Ok(due)
    }

    fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        Ok(self
            .outbox_deadlines
            .first_key_value()
            .map(|((at, _, _), _)| *at))
    }

    fn set_lane_waiting(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.replace_lane(
            key,
            expected_revision,
            if auth {
                LaneState::WaitingAuth
            } else {
                LaneState::WaitingConnection
            },
        )
    }

    fn set_lane_eligible(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.replace_lane(key, expected_revision, LaneState::Eligible { since })
    }

    fn set_lane_transient(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    ) -> Result<RecoveredLane, PersistenceError> {
        if raw_reason
            .as_ref()
            .is_some_and(|reason| reason.len() > 4_096)
        {
            return Err(PersistenceError(
                "transient raw reason exceeds 4096 bytes".into(),
            ));
        }
        let lane = self
            .get_lane(key)
            .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
        if ordinal != lane.last_ordinal {
            return Err(PersistenceError("stale attempt ordinal".into()));
        }
        if ordinal > 0
            && !self.outbox_attempt_details.contains_key(&(
                key.intent_id,
                key.relay.clone(),
                ordinal,
            ))
        {
            return Err(PersistenceError("attempt detail row not found".into()));
        }
        let recovered = self.replace_lane(
            key,
            expected_revision,
            LaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason: raw_reason.clone(),
            },
        )?;
        if ordinal > 0 {
            self.outbox_attempt_details
                .get_mut(&(key.intent_id, key.relay.clone(), ordinal))
                .expect("validated detail")
                .transient = Some(AttemptTransientDetail {
                eligible_at,
                cause,
                raw_reason,
            });
        }
        Ok(recovered)
    }

    fn start_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        event: Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
        let lane = self
            .get_lane(key)
            .cloned()
            .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
        if lane.revision != expected_revision || !matches!(lane.state, LaneState::Eligible { .. }) {
            return Err(PersistenceError(
                "lane is not expected eligible cursor".into(),
            ));
        }
        lane.revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?;
        let intent = self
            .outbox_intents
            .get(&key.intent_id)
            .ok_or_else(|| PersistenceError("attempt intent is not open".into()))?;
        if intent.sig_state != IntentSigState::Signed || intent.frozen != event {
            return Err(PersistenceError(
                "attempt bytes are not the intent's promoted signed bytes".into(),
            ));
        }
        event
            .verify()
            .map_err(|err| PersistenceError(format!("attempt event is invalid: {err}")))?;
        let ordinal = lane
            .last_ordinal
            .checked_add(1)
            .ok_or_else(|| PersistenceError("attempt ordinal exhausted".into()))?;
        let attempt = RecoveredAttempt {
            version: 1,
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            event,
            outcome: AttemptOutcome::Started,
        };
        self.outbox_attempts
            .insert((key.intent_id, key.relay.clone(), ordinal), attempt.clone());
        self.outbox_attempt_details.insert(
            (key.intent_id, key.relay.clone(), ordinal),
            RecoveredAttemptDetails {
                version: 1,
                intent_id: key.intent_id,
                relay: key.relay.clone(),
                ordinal,
                started_at: Some(started_at),
                handoff: None,
                transient: None,
                finished_at: None,
                terminal: None,
            },
        );
        let mut advanced = self.replace_lane(
            key,
            expected_revision,
            LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingHandoff,
            },
        )?;
        advanced.last_ordinal = ordinal;
        self.insert_lane(advanced.clone());
        Ok((attempt, advanced))
    }

    fn record_lane_handoff(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        detail: AttemptHandoffDetail,
        next: PostHandoffState,
    ) -> Result<RecoveredLane, PersistenceError> {
        if matches!(
            &next,
            PostHandoffState::Terminal {
                outcome: AttemptOutcome::Started,
                ..
            }
        ) {
            return Err(PersistenceError("Started is not terminal".into()));
        }
        if matches!(
            &next,
            PostHandoffState::Transient {
                raw_reason: Some(reason),
                ..
            } if reason.len() > 4_096
        ) {
            return Err(PersistenceError(
                "transient raw reason exceeds 4096 bytes".into(),
            ));
        }
        let lane = self
            .get_lane(key)
            .cloned()
            .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
        if lane.revision != expected_revision || lane.last_ordinal != ordinal {
            return Err(PersistenceError("stale lane handoff".into()));
        }
        if !matches!(
            lane.state,
            LaneState::InFlight {
                ordinal: current,
                phase: InFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return Err(PersistenceError("lane is not awaiting handoff".into()));
        }
        lane.revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?;
        let details = self
            .outbox_attempt_details
            .get_mut(&(key.intent_id, key.relay.clone(), ordinal))
            .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
        if let Some(existing) = &details.handoff {
            if existing != &detail {
                return Err(PersistenceError("conflicting handoff evidence".into()));
            }
        } else {
            details.handoff = Some(detail);
        }
        let state = match next {
            PostHandoffState::WaitingConnection => LaneState::WaitingConnection,
            PostHandoffState::WaitingAuth => LaneState::WaitingAuth,
            PostHandoffState::Eligible { since } => LaneState::Eligible { since },
            PostHandoffState::AwaitingAck { deadline } => LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingAck { deadline },
            },
            PostHandoffState::Transient {
                eligible_at,
                cause,
                raw_reason,
            } => LaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason,
            },
            PostHandoffState::Terminal {
                outcome,
                finished_at,
            } => {
                if outcome == AttemptOutcome::Started {
                    return Err(PersistenceError("Started is not terminal".into()));
                }
                details.finished_at = Some(finished_at);
                details.terminal = Some(outcome.clone());
                LaneState::Terminal { ordinal, outcome }
            }
        };
        self.replace_lane(key, expected_revision, state)
    }

    fn finish_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        if outcome == AttemptOutcome::Started {
            return Err(PersistenceError("Started is not terminal".into()));
        }
        let lane = self
            .get_lane(key)
            .cloned()
            .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
        if lane.revision != expected_revision || lane.last_ordinal != ordinal {
            return Err(PersistenceError("stale terminal attempt".into()));
        }
        lane.revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?;
        let details = self
            .outbox_attempt_details
            .get_mut(&(key.intent_id, key.relay.clone(), ordinal))
            .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
        if let Some(existing) = &details.terminal {
            if existing == &outcome
                && details.finished_at == Some(finished_at)
                && matches!(
                    lane.state,
                    LaneState::Terminal {
                        ordinal: current,
                        outcome: ref current_outcome,
                    } if current == ordinal && current_outcome == &outcome
                )
            {
                return Ok(lane);
            }
            return Err(PersistenceError(
                "attempt already has conflicting terminal evidence".into(),
            ));
        }
        details.finished_at = Some(finished_at);
        details.terminal = Some(outcome.clone());
        self.replace_lane(
            key,
            expected_revision,
            LaneState::Terminal { ordinal, outcome },
        )
    }

    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttemptDetails>, PersistenceError> {
        Ok(self
            .outbox_attempt_details
            .values()
            .filter(|detail| detail.intent_id == intent_id)
            .cloned()
            .collect())
    }

    fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        if !self.outbox_intents.contains_key(&intent_id) {
            return Ok(CloseIntentOutcome::AlreadyClosed);
        }
        let lanes = self.recover_outbox_lanes(intent_id)?;
        if lanes.is_empty()
            || lanes
                .iter()
                .any(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
        {
            return Err(PersistenceError(
                "intent lanes are not non-empty and terminal".into(),
            ));
        }
        if let Some(rows) = self.outbox_deadlines_by_intent.remove(&intent_id) {
            for (at, relay) in rows {
                self.outbox_deadlines.remove(&(at, intent_id, relay));
            }
        }
        self.outbox_intents.remove(&intent_id);
        Ok(CloseIntentOutcome::Closed)
    }

    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError> {
        // Receipt-ONLY: no EVENTS row, no OUTBOX_INTENTS row — nothing
        // backs `intent_id` at all (`None`). `MemoryStore` never models a
        // real crash (Q4), so there is no boot-time reconciliation to
        // `Abandoned` here — an ephemeral receipt just stays `Accepted`
        // for the life of the process unless the engine transitions it
        // itself (out of this unit's scope).
        let receipt_id = self.alloc_receipt_id()?;
        self.outbox_receipts.insert(
            receipt_id,
            RecoveredReceipt {
                receipt_id,
                intent_id: None,
                frozen_id,
                expected_pubkey,
                state: ReceiptState::Accepted,
            },
        );
        Ok(receipt_id)
    }
}

#[cfg(test)]
mod lane_atomicity_tests {
    use super::*;
    use crate::{sentinel_signature, AcceptWrite};
    use nostr::{EventBuilder, Keys};

    fn setup(content: &str) -> (MemoryStore, IntentId, RelayUrl, Event, RecoveredLane) {
        let keys = Keys::generate();
        let signed = EventBuilder::new(Kind::TextNote, content)
            .custom_created_at(Timestamp::from(900u64))
            .sign_with_keys(&keys)
            .unwrap();
        let frozen = Event::new(
            signed.id,
            signed.pubkey,
            signed.created_at,
            signed.kind,
            signed.tags.clone(),
            signed.content.clone(),
            sentinel_signature(),
        );
        let relay = RelayUrl::parse(&format!("wss://{content}.atomic.example")).unwrap();
        let mut store = MemoryStore::new();
        let accepted = store
            .accept_write(AcceptWrite {
                frozen,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "atomic".into(),
                durability: WriteDurability::Durable,
                routing: "atomic".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(900u64),
            })
            .unwrap();
        let intent = accepted.journaled_intent_id().unwrap();
        store.promote_signed(intent, signed.sig).unwrap();
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .unwrap();
        let lane = store.bootstrap_outbox_lanes(intent).unwrap().remove(0);
        (store, intent, relay, signed, lane)
    }

    fn exhaust(store: &mut MemoryStore, intent: IntentId, relay: &RelayUrl) {
        store
            .outbox_lanes
            .get_mut(&intent)
            .unwrap()
            .get_mut(relay)
            .unwrap()
            .revision = u64::MAX;
    }

    fn assert_lane_state_unchanged(
        store: &MemoryStore,
        lanes: &BTreeMap<IntentId, BTreeMap<RelayUrl, RecoveredLane>>,
        deadlines: &BTreeMap<(Timestamp, IntentId, RelayUrl), LaneDeadline>,
        deadlines_by_intent: &BTreeMap<IntentId, BTreeSet<(Timestamp, RelayUrl)>>,
    ) {
        assert_eq!(&store.outbox_lanes, lanes);
        assert_eq!(&store.outbox_deadlines, deadlines);
        assert_eq!(&store.outbox_deadlines_by_intent, deadlines_by_intent);
    }

    #[test]
    fn revision_exhaustion_leaves_memory_transition_start_and_finish_atomic() {
        let (mut transition, intent, relay, _, lane) = setup("transition");
        let lane = transition
            .set_lane_transient(
                &lane.key,
                lane.revision,
                0,
                Timestamp::from(950u64),
                TransientCause::ConnectionLost,
                None,
            )
            .unwrap();
        exhaust(&mut transition, intent, &relay);
        let lanes = transition.outbox_lanes.clone();
        let deadlines = transition.outbox_deadlines.clone();
        let deadlines_by_intent = transition.outbox_deadlines_by_intent.clone();
        assert!(transition
            .set_lane_waiting(&lane.key, u64::MAX, false)
            .is_err());
        assert_lane_state_unchanged(&transition, &lanes, &deadlines, &deadlines_by_intent);

        let (mut legacy_start, intent, relay, signed, _) = setup("legacy-start");
        exhaust(&mut legacy_start, intent, &relay);
        let lanes = legacy_start.outbox_lanes.clone();
        let attempts = legacy_start.outbox_attempts.clone();
        let details = legacy_start.outbox_attempt_details.clone();
        assert!(legacy_start.start_attempt(intent, relay, signed).is_err());
        assert_eq!(legacy_start.outbox_lanes, lanes);
        assert_eq!(legacy_start.outbox_attempts, attempts);
        assert_eq!(legacy_start.outbox_attempt_details, details);

        let (mut new_start, intent, relay, signed, lane) = setup("new-start");
        let lane = new_start
            .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(901u64))
            .unwrap();
        exhaust(&mut new_start, intent, &relay);
        let lanes = new_start.outbox_lanes.clone();
        let attempts = new_start.outbox_attempts.clone();
        let details = new_start.outbox_attempt_details.clone();
        assert!(new_start
            .start_lane_attempt(&lane.key, u64::MAX, signed, Timestamp::from(902u64))
            .is_err());
        assert_eq!(new_start.outbox_lanes, lanes);
        assert_eq!(new_start.outbox_attempts, attempts);
        assert_eq!(new_start.outbox_attempt_details, details);

        let (mut finish, intent, relay, signed, _) = setup("finish");
        finish.start_attempt(intent, relay.clone(), signed).unwrap();
        exhaust(&mut finish, intent, &relay);
        let lanes = finish.outbox_lanes.clone();
        let details = finish.outbox_attempt_details.clone();
        assert!(finish
            .finish_attempt(intent, &relay, 1, AttemptOutcome::Acked)
            .is_err());
        assert_eq!(finish.outbox_lanes, lanes);
        assert_eq!(finish.outbox_attempt_details, details);
    }

    #[test]
    fn bootstrap_rejects_cross_table_terminal_state_contradictions_in_memory() {
        let (mut terminal, intent, relay, signed, _) = setup("terminal-mismatch");
        terminal
            .start_attempt(intent, relay.clone(), signed)
            .unwrap();
        terminal
            .finish_attempt(intent, &relay, 1, AttemptOutcome::Acked)
            .unwrap();
        terminal
            .outbox_lanes
            .get_mut(&intent)
            .unwrap()
            .get_mut(&relay)
            .unwrap()
            .state = LaneState::WaitingConnection;
        assert!(terminal
            .bootstrap_outbox_lanes(intent)
            .unwrap_err()
            .to_string()
            .contains("terminal attempt and lane"));

        let (mut live, intent, relay, signed, _) = setup("live-mismatch");
        live.start_attempt(intent, relay.clone(), signed).unwrap();
        live.outbox_lanes
            .get_mut(&intent)
            .unwrap()
            .get_mut(&relay)
            .unwrap()
            .state = LaneState::Terminal {
            ordinal: 1,
            outcome: AttemptOutcome::Acked,
        };
        assert!(live
            .bootstrap_outbox_lanes(intent)
            .unwrap_err()
            .to_string()
            .contains("terminal lane lacks"));
    }
}
