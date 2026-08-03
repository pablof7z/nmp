//! One observation's per-source ACQUISITION EVIDENCE, read as facts.
//!
//! Apart from [`super::observe`] deliberately. That module owns ACCUMULATION
//! -- the channels a scenario folds and the bounded observers a `Then` reads
//! them through. This one owns INTERPRETATION: given the per-branch,
//! per-source facts a frame already carries, what does a step or a settle
//! wait get to conclude from them? Four step files and the wire-settle wait
//! ask that independently of any accumulator, and #12's whole point is that
//! the answer is a fold over per-source facts rather than a verdict anyone
//! stores -- so the fold belongs somewhere a reader can find it by name.
//!
//! Nothing here is a surface addition. `crates/nmp/src/core/evidence.rs`
//! forbids NMP itself shipping an `is_complete`-style aggregate and says an
//! app rolls the per-source facts into its own progress policy; this module
//! is this harness being that app.

use nmp::mechanism::core::{AcquisitionEvidence, ShortfallFact, SourceEvidence};

/// Every source fact across every canonical branch of one observation.
///
/// The frame keeps branch identity; a step that asks "is any planned source
/// still unproven" asks it of the whole observation, which is the union of
/// its branches' own scoped facts -- never a rolled-up verdict stored
/// anywhere.
pub(crate) fn branch_sources(
    evidence: &[AcquisitionEvidence],
) -> impl Iterator<Item = &SourceEvidence> {
    evidence.iter().flat_map(|branch| branch.sources.iter())
}

/// Every shortfall fact across every canonical branch of one observation.
pub(crate) fn branch_shortfall(
    evidence: &[AcquisitionEvidence],
) -> impl Iterator<Item = &ShortfallFact> {
    evidence.iter().flat_map(|branch| branch.shortfall.iter())
}

/// Has every source that covers any atom of this observation's subtree proven
/// all of them?
///
/// `SourceEvidence::reconciled_through` is `Some` only when EVERY subtree atom
/// that source covers has a durable coverage row at or below the query's
/// window floor -- and #12 put the INTERIOR `Derived` atoms in that subtree,
/// so it stays `None` until the inner demand's own request has settled AND
/// every outer atom the inner rows resolved to has been requested and settled
/// too. That is the real "the derived set finished resolving" condition,
/// produced by the component that owns coverage, rather than inferred from a
/// quiet outbound socket that cannot observe ingestion at all (#1211).
///
/// A HARNESS-side rollup of per-source facts -- exactly the "an app rolls
/// per-source facts into its own progress policy" that
/// `crates/nmp/src/core/evidence.rs` reserves to the app. Nothing is added to
/// NMP's surface; there is still no `is_complete` anywhere in it.
///
/// AN EMPTY VIEW IS NOT A PROVEN ONE. Both emptiness checks are load-bearing:
/// evidence arrives on a delta channel, so "nothing delivered yet" and "no
/// source covers anything yet" are both states this is asked about BEFORE the
/// first REQ has even been planned, and a vacuous `all()` over either would
/// return exactly the premature `true` this replaced.
pub(crate) fn every_source_has_proven_its_subtree(evidence: &[AcquisitionEvidence]) -> bool {
    !evidence.is_empty()
        && evidence.iter().all(|branch| {
            !branch.sources.is_empty()
                && branch
                    .sources
                    .iter()
                    .all(|source| source.reconciled_through.is_some())
        })
}

/// Everything a settle wait can say about WHY it gave up, so a timeout names
/// the unproven source rather than failing an assertion downstream of it.
pub(crate) fn unproven_report(evidence: &[AcquisitionEvidence]) -> String {
    if evidence.is_empty() {
        return "no acquisition evidence has been delivered at all".to_owned();
    }
    let mut parts = Vec::new();
    for (branch, evidence) in evidence.iter().enumerate() {
        if evidence.sources.is_empty() {
            parts.push(format!(
                "branch {branch}: no source covers any subtree atom"
            ));
        }
        for source in &evidence.sources {
            if source.reconciled_through.is_none() {
                parts.push(format!(
                    "branch {branch}: {} ({:?}) has proven nothing yet",
                    source.relay, source.status
                ));
            }
        }
        for fact in &evidence.shortfall {
            parts.push(format!("branch {branch}: shortfall {fact:?}"));
        }
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use nmp::mechanism::core::SourceStatus;
    use nostr::{RelayUrl, Timestamp};

    use super::*;

    fn source(reconciled_through: Option<Timestamp>) -> SourceEvidence {
        SourceEvidence {
            relay: RelayUrl::parse("ws://hub.test").expect("a literal relay url parses"),
            access: nmp_grammar::AccessContext::Public,
            reconciled_through,
            status: SourceStatus::Requesting,
        }
    }

    fn branch(sources: Vec<SourceEvidence>) -> AcquisitionEvidence {
        AcquisitionEvidence {
            sources,
            shortfall: Vec::new(),
        }
    }

    /// Falsifier for #1211: a settle that reports "proven" against a view
    /// nothing has been delivered into yet. Every count downstream of it is
    /// then taken before the first REQ exists, which is precisely how a
    /// partially-resolved derived set was read as a finished one.
    #[test]
    fn an_undelivered_or_unplanned_view_is_never_proven() {
        assert!(!every_source_has_proven_its_subtree(&[]));
        assert!(!every_source_has_proven_its_subtree(&[branch(vec![])]));
    }

    /// One unproven source is enough, in any branch. A rollup that min'd or
    /// or'd instead would call a query proven while a whole relay's worth of
    /// its subtree was still outstanding -- the #12 lie, reintroduced by a
    /// harness rather than by the surface.
    #[test]
    fn one_source_short_of_proof_leaves_the_whole_observation_unproven() {
        let proven = source(Some(Timestamp::from(1)));
        assert!(!every_source_has_proven_its_subtree(&[branch(vec![
            proven.clone(),
            source(None),
        ])]));
        assert!(!every_source_has_proven_its_subtree(&[
            branch(vec![proven.clone()]),
            branch(vec![source(None)]),
        ]));
        assert!(every_source_has_proven_its_subtree(&[
            branch(vec![proven.clone()]),
            branch(vec![proven]),
        ]));
    }
}
