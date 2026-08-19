//! Typed ownership for the durable write obligations this reducer holds open,
//! and the three indexes plus one safety valve that mirror them (#1606).
//!
//! Fields are PRIVATE and this is a sibling module of `write`, `lane_projection`
//! and `auth_transport`, so none of them can reach the maps -- only this file
//! can. That is the whole point: the four mirrors below (`intent_receipts`,
//! `event_to_receipts`, `receipts_by_lane_relay`, and each row's own
//! `lane_projection.persisted`) had every one of their drifts start as a caller
//! updating one map and not its partner. Privacy makes that spelling
//! unavailable rather than merely discouraged, and
//! [`PendingWrites::assert_consistent`] proves the mirrors still agree.
//!
//! It holds state and the invariants over that state, and nothing else: no
//! `store`, no `router`, no `resolver`, no `Effect`. Anything that has to emit
//! is orchestration and stays on `CoreState`. This is the `RequestAttempts`
//! contract, verbatim.

use std::collections::{BTreeSet, HashMap};
use std::ops::Index;

use nmp_store::{IntentId, PublishQueueLane};

use super::{EventId, LaneWorkerProjection, PendingWrite, ReceiptId, RelayUrl};

/// Every open durable write obligation, keyed by the receipt that owns it,
/// with the indexes that answer "which receipt owns this intent", "which
/// receipts own these exact frozen bytes", and "which receipts have a lane on
/// this relay" without a scan.
#[derive(Default)]
pub(super) struct PendingWrites {
    /// Publish queue (§3.4 / VISION §7, guarantee #6/#9). Keyed by `ReceiptId`
    /// from `Publish` through to the last terminal per-relay status.
    pending: HashMap<ReceiptId, PendingWrite>,
    /// O(1) reverse index of each row's own `intent_id` (epic #507 finding
    /// E5): [`Self::receipt_for_intent`] used to be a full linear scan of
    /// `pending`, run once per due deadline in
    /// `consume_due_publish_queue_deadlines`. Maintained at every REAL
    /// insertion/removal (never at `fail_and_compensate`'s transient
    /// remove-then-reinsert, which never changes which intent a receipt owns).
    /// This mirrors `pending` exactly and needs no separate invalidation
    /// story: it is rebuilt from scratch, in step with `pending`, every
    /// `recover_on_boot`.
    intent_receipts: HashMap<IntentId, ReceiptId>,
    /// Active durable obligations grouped by their final frozen event id.
    /// Used both to correlate relay OK frames after signing and, #903, to join
    /// an ordinary query row directly to every live receipt that owns those
    /// exact bytes. It includes signer-parked writes from acceptance onward,
    /// excludes terminal retained history, and is rebuilt from the store's
    /// open intents on every boot.
    ///
    /// Unlike `intent_receipts` this is deliberately NOT a mirror of the rows'
    /// current `frozen.id`: a semantic receipt that rode a predecessor
    /// generation to a relay stays named by that predecessor's entry for as
    /// long as it has live work on those bytes.
    event_to_receipts: HashMap<EventId, BTreeSet<ReceiptId>>,
    /// Relay -> receipts with a lane on that relay (epic #507 finding E5).
    /// A narrowing INDEX only, never a second source of truth: the store's
    /// `PUBLISH_QUEUE_LANES` table stays authoritative (its keys are
    /// intent-first, and `close_terminal_intent` deliberately never deletes a
    /// closed intent's own terminal lane rows -- `RedbStore` only drops
    /// `PUBLISH_QUEUE_INTENTS`/the deadline indexes there, per that door's own
    /// doc comment: "Receipts and all route/attempt/detail evidence are
    /// retained" -- so a durable relay-scoped secondary table would still index
    /// retained garbage and would need transactional maintenance across every
    /// lane-writing door).
    ///
    /// This index instead rides the reducer's own `pending`/`recover_on_boot`
    /// lifecycle: rebuilt deterministically at boot, so there is no cache-
    /// invalidation question distinct from the one `pending` itself already
    /// answers. `wake_relay_lanes` uses this to avoid re-reading every
    /// outstanding write's lanes on every relay connect/disconnect/auth event
    /// -- it only narrows WHICH intents to re-read via
    /// `recover_publish_queue_lanes`, the store read itself remains the truth.
    ///
    /// Exactly the union of every row's `lane_projection.persisted`, which is
    /// why every writer of that set lives in this file.
    receipts_by_lane_relay: HashMap<RelayUrl, BTreeSet<ReceiptId>>,
}

/// Reading a row the caller has already proven is live, and panicking when it
/// is not -- the same contract `HashMap` gives, kept because several write-
/// plane steps have already established the row exists two statements earlier
/// and an `unwrap` there would say less than the panic does.
impl Index<&ReceiptId> for PendingWrites {
    type Output = PendingWrite;

    fn index(&self, id: &ReceiptId) -> &PendingWrite {
        self.pending
            .get(id)
            .unwrap_or_else(|| panic!("pending write {id:?} is not live"))
    }
}

impl PendingWrites {
    // -- rows -------------------------------------------------------------

    pub(super) fn get(&self, id: &ReceiptId) -> Option<&PendingWrite> {
        self.pending.get(id)
    }

    /// The one remaining un-narrowed door onto a row's own fields.
    ///
    /// It stays map-shaped on purpose: narrowing it means naming every write
    /// -plane transition over a `PendingWrite`, which is the write-plane
    /// extraction itself rather than this owner's job. What it deliberately
    /// does NOT offer is bulk mutable iteration -- a caller that wants to
    /// touch many rows must say which rows and why, which is how the AUTH
    /// ingest path stopped being an anonymous `iter_mut().find(...)`.
    pub(super) fn get_mut(&mut self, id: &ReceiptId) -> Option<&mut PendingWrite> {
        self.pending.get_mut(id)
    }

    pub(super) fn contains(&self, id: &ReceiptId) -> bool {
        self.pending.contains_key(id)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&ReceiptId, &PendingWrite)> {
        self.pending.iter()
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &PendingWrite> {
        self.pending.values()
    }

    pub(super) fn receipt_ids(&self) -> Vec<ReceiptId> {
        self.pending.keys().copied().collect()
    }

    /// Insert a row WITHOUT touching any index.
    ///
    /// The transient half of `fail_and_compensate`'s remove-then-reinsert:
    /// the obligation and its lanes stay live throughout, so its index entries
    /// must survive untouched. A real admission pairs this with
    /// [`Self::remember_indexes`].
    pub(super) fn insert(&mut self, id: ReceiptId, pending: PendingWrite) {
        self.pending.insert(id, pending);
    }

    /// Remove a row WITHOUT touching any index. Pairs with
    /// [`Self::insert`]; a real discard pairs with [`Self::forget_indexes`].
    pub(super) fn remove(&mut self, id: &ReceiptId) -> Option<PendingWrite> {
        self.pending.remove(id)
    }

    // -- intent index -----------------------------------------------------

    /// O(1) via `intent_receipts` (epic #507 finding E5) -- this door used to
    /// be a full `pending` linear scan, run once per due deadline in
    /// `consume_due_publish_queue_deadlines`.
    pub(super) fn receipt_for_intent(&self, intent_id: IntentId) -> Option<ReceiptId> {
        self.intent_receipts.get(&intent_id).copied()
    }

    /// Name one receipt as the owner of `intent_id` and of these exact frozen
    /// bytes -- I1's INSERTION half, the mirror of [`Self::forget_indexes`].
    ///
    /// Call this at every REAL insertion, never at `fail_and_compensate`'s
    /// transient remove-then-reinsert, which never changes which intent a
    /// receipt owns or which event it materializes. Having only the removal
    /// half be a door is what let three copies of the insertion drift apart
    /// unnoticed.
    ///
    /// `intent_id` is `None` only for Ephemeral, which owns no pending row and
    /// no lane, so there is nothing to index for it (epic #507 finding E5).
    /// `receipts_by_lane_relay` is deliberately absent: no lane exists yet at
    /// insertion time -- a lane is indexed when its projection persists.
    pub(super) fn remember_indexes(
        &mut self,
        id: ReceiptId,
        intent_id: Option<IntentId>,
        event_id: EventId,
    ) {
        let Some(intent_id) = intent_id else {
            return;
        };
        self.intent_receipts.insert(intent_id, id);
        self.index_receipt_under_event(event_id, id);
    }

    /// Adopt one receipt as the owner of an intent whose row is already live.
    ///
    /// The member-rewrite half of a replaceable-operation successor: the row
    /// exists and keeps its identity, but the store just told this reducer
    /// which receipt owns the member intent. Separate from
    /// [`Self::remember_indexes`] because no event indexing belongs with it --
    /// the member's bytes are indexed by the successor rewrite itself.
    pub(super) fn adopt_intent(&mut self, intent_id: IntentId, id: ReceiptId) {
        self.intent_receipts.insert(intent_id, id);
    }

    /// Drop an intent's claim on a receipt when no row is left to forget.
    ///
    /// The defensive half of a superseded generation's retirement: the row is
    /// already gone, so [`Self::forget_indexes`] has nothing to be handed, and
    /// leaving the intent named would let a later deadline resolve to a
    /// receipt that no longer exists.
    pub(super) fn forget_intent(&mut self, intent_id: IntentId) {
        self.intent_receipts.remove(&intent_id);
    }

    /// Remove a permanently-discarded pending write's entries from all three
    /// indexes (epic #507 finding E5, #903).
    ///
    /// Call this at every REAL removal -- never at `fail_and_compensate`'s
    /// transient remove-then-reinsert (`CompensateOutcome::NotFound`/`Err`),
    /// which must leave the indexes untouched because the obligation and its
    /// lanes are still live.
    pub(super) fn forget_indexes(&mut self, id: ReceiptId, pending: &PendingWrite) {
        self.intent_receipts.remove(&pending.intent_id);
        // Every event this receipt is indexed under, not just its current
        // frozen one. A semantic receipt that rode a predecessor generation to
        // a relay is still named by that predecessor's entry, and the index
        // means "this receipt has live work on this event". Leaving a stale
        // name behind used to be invisible only because a semantic receipt
        // could never reach a terminal state; once it settles, the publish-
        // queue projection reads that name and finds a receipt whose payload
        // is no longer `Contributing`.
        self.event_to_receipts.retain(|_, receipts| {
            receipts.remove(&id);
            !receipts.is_empty()
        });
        let persisted = pending.lane_projection.persisted.clone();
        self.update_lane_relay_index(id, &persisted, &BTreeSet::new());
    }

    // -- event index ------------------------------------------------------

    /// Name one receipt as owning these exact frozen bytes.
    ///
    /// The one door into `event_to_receipts`, paired with
    /// [`Self::unindex_receipt_from_event`]. Six sites used to spell this
    /// `entry(id).or_default().insert(receipt)` by hand.
    pub(super) fn index_receipt_under_event(&mut self, event_id: EventId, id: ReceiptId) {
        self.event_to_receipts
            .entry(event_id)
            .or_default()
            .insert(id);
    }

    /// Release one receipt's claim on these frozen bytes, dropping the entry
    /// entirely once no receipt names them.
    ///
    /// Pruning is not housekeeping. `event_to_receipts` answers "which live
    /// obligations own these exact bytes", so an entry surviving with an empty
    /// set asserts that bytes nothing owns are still owned. Three sites used to
    /// spell the removal by hand and only two of them pruned: the successor
    /// rewrite in `write/replaceable_operation.rs` left an empty set under
    /// every retired generation's event id, once per rewrite, until the next
    /// boot recovery (#1606). One door, so the two spellings cannot diverge
    /// again.
    pub(super) fn unindex_receipt_from_event(&mut self, event_id: EventId, id: ReceiptId) {
        let Some(receipts) = self.event_to_receipts.get_mut(&event_id) else {
            return;
        };
        receipts.remove(&id);
        if receipts.is_empty() {
            self.event_to_receipts.remove(&event_id);
        }
    }

    pub(super) fn receipts_for_event(&self, event_id: &EventId) -> Option<&BTreeSet<ReceiptId>> {
        self.event_to_receipts.get(event_id)
    }

    pub(super) fn indexed_events(&self) -> impl Iterator<Item = (&EventId, &BTreeSet<ReceiptId>)> {
        self.event_to_receipts.iter()
    }

    // -- signing ----------------------------------------------------------

    /// Ordinary relay ingest committed the signed bytes this intent was
    /// waiting for: the obligation is signed, and no local signer request is
    /// outstanding for it any more. Returns the receipt that owns the intent
    /// so the caller can run the shared post-signature path.
    ///
    /// The row is found through `intent_receipts` rather than the linear
    /// `pending.iter_mut().find(...)` this replaced -- the same substitution
    /// epic #507 finding E5 already made everywhere else, exact because
    /// [`Self::assert_consistent`] proves that index is a bijection with the
    /// rows' own `intent_id`.
    ///
    /// Two things it deliberately does NOT do, both preserved verbatim:
    ///
    /// - It does not check that a sign request was actually in flight. It does
    ///   not need to: the resolver only reports an intent satisfied once the
    ///   matching row is COMMITTED, and `on_signed` still validates the bytes
    ///   against this row's own frozen template before anything is promoted.
    /// - It does not bump `sign_generation`, which every other out-of-band
    ///   clearer of `sign_request_in_flight` does. That is safe, but only
    ///   because of the other half of the correlation rule: a stale
    ///   `SignerCompleted` is rejected by `!sign_request_in_flight` alone, and
    ///   every path that re-arms signing bumps the generation BEFORE setting
    ///   the flag again, so the outstanding operation can never be mistaken
    ///   for a current one. The signer call that is still in flight when this
    ///   runs is simply wasted, never misapplied.
    pub(super) fn adopt_ingested_signature(&mut self, intent_id: IntentId) -> Option<ReceiptId> {
        let id = self.receipt_for_intent(intent_id)?;
        let pending = self.pending.get_mut(&id)?;
        pending.already_signed = true;
        pending.sign_request_in_flight = false;
        Some(id)
    }

    /// The store promoted this intent's journal atomically alongside another
    /// receipt's signature: advance the in-memory projection to match, so an
    /// offline co-owner is not stranded behind a row that is already validly
    /// signed. Returns the receipt, or `None` when no live row owns the intent.
    pub(super) fn adopt_co_signature(&mut self, intent_id: IntentId) -> Option<ReceiptId> {
        let id = self.receipt_for_intent(intent_id)?;
        let pending = self.pending.get_mut(&id)?;
        pending.already_signed = true;
        Some(id)
    }

    // -- lane projection --------------------------------------------------

    /// Move `receipts_by_lane_relay` from one persisted-relay set to another
    /// for one receipt: drop it from every relay it no longer persists to, add
    /// it to every relay it newly does.
    ///
    /// The ONLY writer of `receipts_by_lane_relay`, in both directions, and
    /// private even within this owner so no door around it can spell the
    /// maintenance a second way. Two of those doors used to insert into the
    /// index directly. That was safe -- they only ever add -- but it made
    /// "the one door" true for removal and false for addition, which is how
    /// the divergence this function exists to end began: one rewrite site
    /// pruning and its sibling not (#1606). A door that is only sometimes the
    /// door is not a door.
    fn update_lane_relay_index(
        &mut self,
        id: ReceiptId,
        previous: &BTreeSet<RelayUrl>,
        next: &BTreeSet<RelayUrl>,
    ) {
        for relay in previous.difference(next) {
            if let Some(receipts) = self.receipts_by_lane_relay.get_mut(relay) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.receipts_by_lane_relay.remove(relay);
                }
            }
        }
        for relay in next.difference(previous) {
            self.receipts_by_lane_relay
                .entry(relay.clone())
                .or_default()
                .insert(id);
        }
    }

    /// Replace one receipt's projection from a complete recovered lane set.
    ///
    /// Bootstrap returns every retained lane for the intent, so this is an
    /// exact rebuild rather than an incremental merge. Returns `false` when no
    /// row owns the receipt -- the caller's projection gap, not this owner's.
    pub(super) fn replace_lane_projection(
        &mut self,
        id: ReceiptId,
        lanes: &[PublishQueueLane],
    ) -> bool {
        let Some(previous) = self
            .pending
            .get(&id)
            .map(|pending| pending.lane_projection.persisted.clone())
        else {
            return false;
        };
        let next = LaneWorkerProjection::from_recovered(lanes);
        self.update_lane_relay_index(id, &previous, &next.persisted);
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.lane_projection = next;
        }
        true
    }

    /// Reset one pending write's lane projection to empty ahead of a
    /// replaceable-operation successor rewrite, releasing every relay it
    /// currently persists to.
    ///
    /// A member rewritten onto a new generation owns no lane the old
    /// generation minted: nothing may re-attach to them, so the projection
    /// resets to empty exactly as [`Self::replace_lane_projection`] would
    /// reset it against an empty recovered lane set. Both call sites that
    /// rewrite an existing member in place used to assign
    /// `LaneWorkerProjection::default()` to the field directly and never told
    /// the index, so every relay the old generation had persisted lanes on
    /// kept naming the receipt until the next full boot recovery (#1606).
    pub(super) fn reset_lane_projection(&mut self, id: ReceiptId) {
        let previous = self
            .pending
            .get(&id)
            .map(|pending| pending.lane_projection.persisted.clone())
            .unwrap_or_default();
        self.update_lane_relay_index(id, &previous, &BTreeSet::new());
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.lane_projection = LaneWorkerProjection::default();
        }
    }

    /// Apply one successful store mutation's exact post-state. Returns `false`
    /// when no row owns the receipt.
    pub(super) fn apply_committed_lane(&mut self, id: ReceiptId, lane: &PublishQueueLane) -> bool {
        let Some(pending) = self.pending.get_mut(&id) else {
            return false;
        };
        if pending.lane_projection.apply(lane) {
            self.update_lane_relay_index(
                id,
                &BTreeSet::new(),
                &BTreeSet::from([lane.key.relay.clone()]),
            );
        }
        true
    }

    /// Which receipts have a lane on this relay, narrowing a wake to the
    /// intents worth re-reading. Never a source of truth -- the store's lane
    /// rows remain that.
    pub(super) fn receipts_with_lane_on(&self, relay: &RelayUrl) -> Vec<ReceiptId> {
        self.receipts_by_lane_relay
            .get(relay)
            .map(|receipts| receipts.iter().copied().collect())
            .unwrap_or_default()
    }

    // -- proofs -----------------------------------------------------------

    /// Exact structural consistency for every mirror this owner keeps, by
    /// identity rather than by count.
    ///
    /// `counts()` next to this counts things -- the right instrument for leaks
    /// and boundedness, and the wrong one for structure: a receipt indexed
    /// under another receipt's intent, or a lane filed under the wrong relay,
    /// preserves every number `counts()` reports.
    ///
    /// Three mirrors, and they are not the same KIND of mirror, which is why
    /// only two of them are checked in both directions:
    ///
    /// - `intent_receipts` is an exact bijection with the rows' own
    ///   `intent_id`. Both directions.
    /// - `receipts_by_lane_relay` is exactly the union of the rows'
    ///   `lane_projection.persisted`. Both directions.
    /// - `event_to_receipts` is deliberately NOT a mirror of `frozen.id`: a
    ///   receipt keeps its predecessor generation's entry while it still has
    ///   live work on those bytes. Only what it can still get wrong is
    ///   checked -- naming a receipt that is not live, or keeping an empty set
    ///   that claims unowned bytes are owned.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str) {
        for (id, pending) in &self.pending {
            let owner = self
                .intent_receipts
                .get(&pending.intent_id)
                .unwrap_or_else(|| {
                    panic!(
                        "{at}: pending write {id:?} has no intent_receipts entry for its own intent {:?}",
                        pending.intent_id
                    )
                });
            assert_eq!(
                owner, id,
                "{at}: intent_receipts for intent {:?} names a receipt that does not own it",
                pending.intent_id
            );
            for relay in &pending.lane_projection.persisted {
                let receipts = self.receipts_by_lane_relay.get(relay).unwrap_or_else(|| {
                    panic!(
                        "{at}: pending write {id:?} persists a lane on {relay} that the lane index does not know"
                    )
                });
                assert!(
                    receipts.contains(id),
                    "{at}: pending write {id:?} is not named by the lane index for its own relay {relay}"
                );
            }
        }
        for (intent_id, id) in &self.intent_receipts {
            let pending = self.pending.get(id).unwrap_or_else(|| {
                panic!("{at}: intent_receipts names receipt {id:?}, which is not live")
            });
            assert_eq!(
                &pending.intent_id, intent_id,
                "{at}: receipt {id:?} is indexed under an intent it does not report"
            );
        }
        for (event_id, receipts) in &self.event_to_receipts {
            assert!(
                !receipts.is_empty(),
                "{at}: pending writes kept an empty event_to_receipts set for {event_id}"
            );
            for id in receipts {
                assert!(
                    self.pending.contains_key(id),
                    "{at}: event_to_receipts names receipt {id:?} for {event_id}, which is not live"
                );
            }
        }
        for (relay, receipts) in &self.receipts_by_lane_relay {
            assert!(
                !receipts.is_empty(),
                "{at}: pending writes kept an empty lane index set for {relay}"
            );
            for id in receipts {
                let pending = self.pending.get(id).unwrap_or_else(|| {
                    panic!(
                        "{at}: the lane index names receipt {id:?} on {relay}, which is not live"
                    )
                });
                assert!(
                    pending.lane_projection.persisted.contains(relay),
                    "{at}: receipt {id:?} is indexed as having a lane on {relay}, which it does not persist"
                );
            }
        }
    }
}

