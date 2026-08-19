//! Per-lane coordinate coverage: acquisition, release, and the parked-coverage query the engine's message door consults.

use super::*;

impl CoreState {
    /// Ask the ordinary query owner whether this relay's current value for
    /// `coordinate` is known before the lane takes a publish attempt.
    ///
    /// Returns `true` when the lane may send. A `false` parks it: the answer
    /// is outstanding, and the lane re-asks on the next scheduling pass
    /// after frames arrive. No verdict is remembered between passes and no
    /// request identity is persisted -- a restart repeats the check.
    ///
    /// `Witnessed` is a green light rather than a comparison against the
    /// pending generation's own source revision, and that is deliberate. A
    /// witnessed event has already been INGESTED, so
    /// `install_semantic_source_successor` has already had it and either
    /// rebased this coordinate onto it -- in which case this lane's
    /// generation is superseded and this lane no longer exists -- or
    /// declined it as stale or as one of NMP's own previously published
    /// materializations. Re-deciding "is this newer" here from `created_at`
    /// alone would park the lane forever on the relay's echo of NMP's own
    /// earlier generation, which is newer than the base that generation was
    /// built from and yet is not a newer list at all.
    pub(super) fn coordinate_is_current_for_lane(
        &mut self,
        receipt: ReceiptId,
        coordinate: &Coordinate,
        session: &RelaySessionKey,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) -> bool {
        // A lane that already owns an open coordinate observation re-reads
        // the answer; it must never open a second REQ for the same question.
        // The door's own duplicate check cannot see one this same turn just
        // asked for, because the reducer mints its REQs before admission
        // flushes them.
        let key = (receipt, relay.clone());
        let already_asked = self.semantic_publish_coverage.contains_key(&key);
        let (coverage, opened) = if already_asked {
            (self.coordinate_coverage(coordinate, session), None)
        } else {
            self.open_coordinate_observation(coordinate, session, effects)
        };
        if let Some(observation) = opened {
            self.semantic_publish_coverage
                .insert(key.clone(), observation);
        }
        match coverage {
            CoordinateCoverage::Witnessed { .. } | CoordinateCoverage::ProvenAbsent => {
                // The question is answered, so the lane stops owning it. A
                // later generation on this same lane asks again from
                // scratch: coverage is a fact about one relay session at one
                // moment, and holding the observation open to reuse a
                // verdict earned before a reconnect would be reading a
                // stale one.
                self.release_coordinate_coverage(receipt, relay, effects);
                true
            }
            // Both mean the question is asked and the answer is coming, so
            // both wait. The barrier case is the one #1683 closed: it used to
            // read as `Uncovered` and publish over a base this relay may have
            // superseded, which is terminal loss. Waiting here is bounded by
            // the barrier's own reconciliation, not open-ended -- the request
            // exists, and its coverage credit is what wakes this lane again.
            CoordinateCoverage::InFlight { .. } | CoordinateCoverage::Reconciling { .. } => {
                self.semantic_publish_coverage_parked.insert(key);
                false
            }
            // Nothing in the ordinary query owner's LIVE bookkeeping covers
            // the coordinate. That is not the same as "this relay is
            // unknown": #1630's door deliberately reads only live-wire-request
            // evidence, which a reconnect or a restart empties while the
            // durable record of what each relay was seen to carry survives.
            //
            // Measured shape when this fires (traced across the semantic
            // delivery witnesses): the relay's read session is CONNECTED and
            // this lane's observation is ALIVE, yet the resolver minted no
            // request for its demand and none is pending admission. The
            // session-death case is not this one -- that is released by
            // `release_coordinate_coverage_for_relay` and re-asked on the
            // session that replaces it.
            //
            // #1683 narrowed this and did not close it. The measured cause
            // of the original window was a `limit: 0` request that answers
            // nothing on its own, which the door could not tell from
            // "nothing ever asked". That case is `Reconciling` and is
            // handled above.
            //
            // What is left here is the residual state, and its cause IS now
            // established (measured, not guessed): a covering REQ can reach
            // `Finished` with its coverage authority POISONED --
            // `CoveragePoison::{LimitedRequest,EventCommitFailed}`
            // (`core/attribution.rs`) -- so `persist_attributed_completion`
            // retires it with `committed_interval: None`
            // (`core/query.rs::persist_attributed_completion`). A poisoned
            // Finished request proves neither presence nor absence, and this
            // door's `Finished` arm only ever tries `proves_absence`, so it
            // silently contributes nothing -- indistinguishable from "no
            // request exists" to a caller reading `Uncovered`. Falsifier:
            // `a_poisoned_finished_coordinate_request_is_read_as_uncovered`
            // reaches this exact state through a real publish and confirms
            // it, by injecting the `EventCommitFailed` poison the way a
            // genuine store commit failure would (never by calling this
            // door directly).
            //
            // The escape below is still load-bearing, and a safer-looking
            // fix was tried and rejected: releasing this lane's stale
            // observation and re-asking (`open_coordinate_observation`
            // again) whenever `already_asked` reads a fresh `Uncovered`,
            // instead of sending, deterministically stalls
            // `relay_source_successors_resume_current_delivery_and_stay_open_after_restart`
            // and `source_session_replacement_wakes_every_signed_successor_destination`
            // -- not by hanging forever, but by never letting an in-flight
            // request reach its own credit before this door's retry tears it
            // down and restarts, repeating indefinitely. Simply always parking
            // has the same failure shape in the other direction (the
            // original "follow that can never leave" defect). Neither
            // alternative was made safe within this pass.
            //
            // Sending is still chosen for it, on the same reasoning #1631
            // used: pre-#1631 EVERY semantic publish went out with no
            // coordinate check at all. `docs/known-gaps.md` records what
            // remains open.
            CoordinateCoverage::Uncovered => {
                if already_asked && !self.wire_admission_needed() {
                    self.release_coordinate_coverage(receipt, relay, effects);
                    return true;
                }
                self.semantic_publish_coverage_parked.insert(key);
                false
            }
        }
    }

    /// Close one lane's coordinate question and withdraw the observation it
    /// opened, if any.
    pub(super) fn release_coordinate_coverage(
        &mut self,
        receipt: ReceiptId,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) {
        let key = (receipt, relay.clone());
        self.semantic_publish_coverage_parked.remove(&key);
        if let Some(observation) = self.semantic_publish_coverage.remove(&key) {
            self.retired_coverage_observations.insert(observation);
            effects.extend(self.on_unsubscribe(observation));
        }
    }

    /// Close every coordinate question outstanding at one relay.
    pub(in crate::core) fn release_coordinate_coverage_for_relay(
        &mut self,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) {
        let owned: Vec<_> = self
            .semantic_publish_coverage
            .keys()
            .chain(self.semantic_publish_coverage_parked.iter())
            .filter(|(_, candidate)| candidate == relay)
            .cloned()
            .collect();
        for (receipt, relay) in owned {
            self.release_coordinate_coverage(receipt, &relay, effects);
        }
    }

    /// Close every coordinate question one receipt still owns.
    ///
    /// The observation this reducer opened belongs to the lane, not to any
    /// app subscription, so nothing else would ever withdraw it. Dropping
    /// the id without unsubscribing leaks a live REQ for the lifetime of
    /// the process.
    pub(super) fn release_all_coordinate_coverage(&mut self, receipt: ReceiptId, effects: &mut Vec<Effect>) {
        self.semantic_publish_coverage_parked
            .retain(|(owner, _)| *owner != receipt);
        let owned: Vec<_> = self
            .semantic_publish_coverage
            .keys()
            .filter(|(owner, _)| *owner == receipt)
            .cloned()
            .collect();
        for (_, relay) in owned {
            self.release_coordinate_coverage(receipt, &relay, effects);
        }
    }

    /// Drop the private delivery effects of every coverage observation this
    /// reducer owns. Everything else keeps its ordinary runtime path.
    pub(in crate::core) fn consume_coverage_observation_effects(
        &mut self,
        effects: Vec<Effect>,
    ) -> Vec<Effect> {
        if self.semantic_publish_coverage.is_empty()
            && self.retired_coverage_observations.is_empty()
        {
            return effects;
        }
        let owned: BTreeSet<ObservationId> = self
            .semantic_publish_coverage
            .values()
            .copied()
            .chain(self.retired_coverage_observations.iter().copied())
            .collect();
        let outward = effects
            .into_iter()
            .filter(|effect| match effect {
                Effect::EmitRows(id, ..) | Effect::RequestSettled(id, _) => !owned.contains(id),
                _ => true,
            })
            .collect();
        self.retired_coverage_observations.clear();
        outward
    }

    /// Whether any semantic publish lane is waiting on a coordinate answer.
    ///
    /// The engine's message door consults this to decide whether the turn it
    /// just reduced could have answered someone, rather than re-running the
    /// whole publish scheduler on every message.
    pub(in crate::core) fn has_parked_coordinate_coverage(&self) -> bool {
        !self.semantic_publish_coverage_parked.is_empty()
    }
}
