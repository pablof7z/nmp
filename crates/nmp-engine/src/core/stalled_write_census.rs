//! The stalled-obligation change detector and the projection it caches
//! (#1743).

use std::collections::BTreeSet;

use nmp_grammar::{RelaySessionKey};

use super::diagnostics::{stalled_write_id, STALLED_WRITE_DETAIL_LIMIT};
use super::pending_writes::PendingWrites;
use super::{
    PendingWrite, ReceiptId, RelayUrl, StalledWrite, StalledWriteStage, StalledWriteTotals,
};

/// Everything the stalled-stage decision reads, named rather than reached
/// for.
///
/// The decision used to be an `&CoreState` method, which made it look like
/// it could consult anything the reducer owns. It consults exactly two
/// things, and one of them is not on the write plane at all: `connected` is
/// transport-session state. Stating it here is what makes the cross-plane
/// dependency visible — and what makes the staleness question askable, since
/// the change detector below is driven by receipt-shaped news while this
/// half of the input can change with no receipt involved.
pub(super) struct StalledWriteInputs<'a> {
    /// Every open durable obligation this reducer owns.
    pub(super) pending: &'a PendingWrites,
    /// Sessions CURRENTLY connected. Only reachability is read from it.
    pub(super) connected: &'a BTreeSet<RelaySessionKey>,
}

/// Where one open obligation is stuck, if it is stuck at all (#756/#968).
///
/// A pure function of the obligation and the current connectivity — no store
/// read, no second retry ledger, no clock, and no re-derivation of anything
/// the write plane did not already commit. The three stages are asked in
/// lifecycle order, because a write with no signature has no route to be
/// missing and a write with no route has no destination to be unreachable.
pub(super) fn stalled_write_stage(
    pending: &PendingWrite,
    connected: &BTreeSet<RelaySessionKey>,
) -> Option<(StalledWriteStage, String)> {
    if !pending.target.accepts_ordinary_signer() {
        return None;
    }
    if pending.event_id.is_none() && !pending.already_signed {
        // A signer request still outstanding is work in progress, not a
        // stall. Only the durable `AwaitingCapability` park -- request
        // answered "no capability", nothing left running -- is stuck, and
        // it names the FROZEN author rather than whichever account is current now.
        if pending.sign_request_in_flight {
            return None;
        }
        return Some((
            StalledWriteStage::Unsignable,
            format!(
                "no signer is registered for {}",
                pending.signing_pubkey.to_hex()
            ),
        ));
    }

    if pending.durable_routes.is_empty() {
        // Parked with nothing resolved. This is the ONE stall that no
        // clock may ever end (#1136): "we have not learned where this
        // goes" is ignorance, and a deadline over ignorance is a verdict.
        // It is reported so an operator can see it, never so anything
        // can abandon it.
        return Some((
            StalledWriteStage::Unroutable,
            "no destination has been resolved yet".to_string(),
        ));
    }

    // Destinations exist. Stuck iff nothing is in flight and not one of
    // them is a relay this process currently holds a session to -- the
    // `wss://non-existent.example` case, and every ordinary outage.
    if !pending.pending_relays.is_empty() {
        return None;
    }
    let access = Some(pending.signing_pubkey);
    let live: BTreeSet<&RelayUrl> = pending
        .lane_projection
        .required_relays()
        .collect();
    if live.is_empty() {
        return None;
    }
    if live
        .iter()
        .any(|relay| connected.contains(&RelaySessionKey::new((*relay).clone(), access)))
    {
        return None;
    }
    let named = live
        .iter()
        .map(|relay| relay.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some((
        StalledWriteStage::Undeliverable,
        format!("no destination is reachable: {named}"),
    ))
}

/// The stalled-obligation census as of the last diagnostics snapshot pushed
/// for a write-plane reason, plus the bounded projection materialized from
/// it.
///
/// Fields are PRIVATE and this is a sibling module of `write`, so the one
/// invariant here — *the cached rows and totals are the projection of the
/// census, and the census is `stalled_write_stage` over every open
/// obligation* — is maintained here or not at all (#1743). Before this owner
/// existed the three fields were written at three separate sites and any
/// future site could have updated one without the others; the failure mode
/// is a snapshot that says nothing is stuck while a write is wedged, which
/// is precisely the failure the section exists to prevent.
///
/// A change detector for an observer, never a ledger: it holds no retry
/// state, no history, and no fact that is not re-derivable from `pending` in
/// one pass. Its only job is to keep an ordinary healthy publish from
/// rebuilding an engine-global snapshot at every beat of a lifecycle in
/// which nothing was ever stuck.
///
/// It holds state and its invariant, and nothing else: no `store`, no
/// `clock`, no `router`, no `resolver`, no `Effect`. Deciding WHICH receipts
/// a turn touched, and pushing `Effect::DiagnosticsChanged` when this owner
/// reports a change, are orchestration and stay on `CoreState`.
pub(super) struct StalledWriteCensus {
    /// Receipt -> stage, sorted by receipt. Sorted because the incremental
    /// path binary-searches it; the DISPLAY order is a different order and
    /// lives in `project` below.
    census: Vec<(ReceiptId, StalledWriteStage)>,
    /// Materialized only when the census changes. Diagnostics snapshots can
    /// be requested by unrelated read/query activity, so rebuilding and
    /// sorting every durable write on each request would put the entire
    /// publish queue on the read-plane hot path.
    rows: Vec<StalledWrite>,
    totals: StalledWriteTotals,
}

impl Default for StalledWriteCensus {
    /// The empty census, and the exact state a store-failure recovery resets
    /// to. `detail_limit` is a property of the projection, not of the
    /// population, so it is stated once here rather than agreed on by every
    /// site that clears the cache.
    fn default() -> Self {
        Self {
            census: Vec::new(),
            rows: Vec::new(),
            totals: empty_totals(),
        }
    }
}

impl StalledWriteCensus {
    /// The bounded detail window of the current census.
    pub(super) fn rows(&self) -> &[StalledWrite] {
        &self.rows
    }

    /// Exact counts behind that window, including rows outside it.
    pub(super) fn totals(&self) -> StalledWriteTotals {
        self.totals
    }

    /// Rebuild both halves from scratch. Boot recovery only: `pending` was
    /// just rebuilt from the store, so there is no prior census any
    /// incremental step could be relative to.
    pub(super) fn rebuild(&mut self, inputs: StalledWriteInputs<'_>) {
        self.census = full_census(&inputs);
        let (rows, totals) = project(&inputs);
        self.rows = rows;
        self.totals = totals;
    }

    /// Refresh only the receipts the caller says changed, and report whether
    /// any observable stage changed.
    ///
    /// The full census is rebuilt once at boot; afterward an unrelated write
    /// must not rescan every durable obligation merely to discover that none
    /// of their stalled stages changed. The projection is re-materialized
    /// only on a real change, which is what keeps an ordinary healthy
    /// publish off the diagnostics rebuild path.
    pub(super) fn refresh(
        &mut self,
        touched: &BTreeSet<ReceiptId>,
        inputs: StalledWriteInputs<'_>,
    ) -> bool {
        let mut changed = false;
        for id in touched {
            let next = inputs
                .pending
                .get(id)
                .and_then(|pending| stalled_write_stage(pending, inputs.connected))
                .map(|(stage, _)| stage);
            let position = self
                .census
                .binary_search_by_key(id, |(receipt, _)| *receipt);
            match (position, next) {
                (Ok(index), Some(stage)) if self.census[index].1 == stage => {}
                (Ok(index), Some(stage)) => {
                    self.census[index].1 = stage;
                    changed = true;
                }
                (Ok(index), None) => {
                    self.census.remove(index);
                    changed = true;
                }
                (Err(index), Some(stage)) => {
                    self.census.insert(index, (*id, stage));
                    changed = true;
                }
                (Err(_), None) => {}
            }
        }
        if changed {
            let (rows, totals) = project(&inputs);
            self.rows = rows;
            self.totals = totals;
        }
        changed
    }

    /// Exact agreement with a fresh recompute from the reducer's own state.
    ///
    /// This owner is a cache, not a bidirectional mirror, so "both
    /// directions by identity" (the shape every other owner's
    /// `assert_consistent` uses) does not apply -- there is only one map
    /// here, not two mirroring each other. What this owner can get wrong
    /// instead is STALENESS: a `refresh` that missed a real change leaves
    /// `census`/`rows`/`totals` agreeing with each other while disagreeing
    /// with what `pending`/`connected` actually say right now. Comparing
    /// sizes or totals could not see that -- a refresh that dropped one
    /// receipt's stage update leaves every count exactly right and one row
    /// exactly wrong, filed under the old stage instead of the new one. So
    /// this recomputes both halves from scratch and demands byte-for-byte
    /// equality against the cached ones, which is why it takes the reducer's
    /// own state as an argument rather than the zero-argument shape the
    /// other owners use.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str, inputs: StalledWriteInputs<'_>) {
        let expected_census = full_census(&inputs);
        assert_eq!(
            self.census, expected_census,
            "{at}: the stalled-write change detector disagrees with a fresh census"
        );
        let (expected_rows, expected_totals) = project(&inputs);
        assert_eq!(
            self.rows, expected_rows,
            "{at}: cached stalled-write rows disagree with a fresh projection"
        );
        assert_eq!(
            self.totals, expected_totals,
            "{at}: cached stalled-write totals disagree with a fresh projection"
        );
    }
}

fn empty_totals() -> StalledWriteTotals {
    StalledWriteTotals {
        detail_limit: u64::try_from(STALLED_WRITE_DETAIL_LIMIT).unwrap_or(u64::MAX),
        ..StalledWriteTotals::default()
    }
}

/// Which obligations are stalled, and at which stage — the allocation-light
/// half of [`stalled_write_stage`], used to decide whether a turn changed
/// anything an observer of this section would notice.
///
/// Deliberately not the detail strings or the descriptors: this runs on
/// every write-plane turn, and a change detector that formatted a sentence
/// and hashed two ids per obligation to decide whether to do nothing would
/// cost more than the snapshot it was avoiding.
fn full_census(inputs: &StalledWriteInputs<'_>) -> Vec<(ReceiptId, StalledWriteStage)> {
    let mut census: Vec<(ReceiptId, StalledWriteStage)> = inputs
        .pending
        .iter()
        .filter_map(|(id, pending)| {
            stalled_write_stage(pending, inputs.connected).map(|(stage, _)| (*id, stage))
        })
        .collect();
    census.sort();
    census
}

/// The bounded stalled-write section of the diagnostics snapshot.
///
/// One pass over the reducer's own open obligations produces both the exact
/// totals and the detail window, so a row outside the window still counts —
/// a bound on bytes is never allowed to become a lie about how much is
/// stuck. Ordering is (stage, acceptance instant, descriptor): a documented
/// display order, independent of map iteration and of anything the scheduler
/// reads.
fn project(inputs: &StalledWriteInputs<'_>) -> (Vec<StalledWrite>, StalledWriteTotals) {
    let mut totals = empty_totals();
    let mut rows = Vec::new();
    for pending in inputs.pending.values() {
        let Some((stage, detail)) = stalled_write_stage(pending, inputs.connected) else {
            continue;
        };
        let counter = match stage {
            StalledWriteStage::Unroutable => &mut totals.unroutable,
            StalledWriteStage::Unsignable => &mut totals.unsignable,
            StalledWriteStage::Undeliverable => &mut totals.undeliverable,
        };
        *counter = counter.saturating_add(1);
        let intent_id = pending.intent_id;
        rows.push(StalledWrite {
            id: stalled_write_id(intent_id.0, &pending.frozen.id),
            stage,
            detail,
            stalled_since: pending.accepted_at,
        });
    }
    rows.sort_by(|a, b| {
        a.stage
            .cmp(&b.stage)
            .then(a.stalled_since.cmp(&b.stalled_since))
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    rows.truncate(STALLED_WRITE_DETAIL_LIMIT);
    totals.omitted_details = total.saturating_sub(u64::try_from(rows.len()).unwrap_or(u64::MAX));
    (rows, totals)
}
