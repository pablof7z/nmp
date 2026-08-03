use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use nmp::{
    AcquisitionEvidence, EventId, Frame, ObservationEvidence, RelayUrl, Row, RowDelta, Subscription,
};

#[derive(Default)]
pub struct Observed {
    pub rows: BTreeMap<EventId, Row>,
    pub evidence: Vec<AcquisitionEvidence>,
    pub execution: Vec<ObservationEvidence>,
    pub source_growth: BTreeSet<EventId>,
    pub frames: usize,
    pub delta_entries: usize,
}

impl Observed {
    pub fn apply(&mut self, frame: Frame) {
        self.frames += 1;
        self.delta_entries += frame.deltas.len();
        self.evidence = frame.evidence;
        self.execution.extend(frame.execution);
        for delta in frame.deltas {
            match delta {
                RowDelta::Added(row) => {
                    self.rows.insert(row.event.id, row);
                }
                RowDelta::SourcesGrew { id, sources } => {
                    self.source_growth.insert(id);
                    if let Some(row) = self.rows.get_mut(&id) {
                        row.sources = sources;
                    }
                }
                RowDelta::Removed(id) => {
                    self.rows.remove(&id);
                }
            }
        }
    }

    pub fn rows_of_kind(&self, kind: u16) -> Vec<&Row> {
        self.rows
            .values()
            .filter(|row| row.event.kind.as_u16() == kind)
            .collect()
    }

    pub fn has_source_count(&self, content: &str, count: usize) -> bool {
        self.rows
            .values()
            .any(|row| row.event.content == content && row.sources.len() == count)
    }

    pub fn relays_in_evidence(&self) -> BTreeSet<RelayUrl> {
        self.evidence
            .iter()
            .flat_map(|branch| branch.sources.iter())
            .map(|source| source.relay.clone())
            .collect()
    }
}

pub fn wait_until(
    subscription: &Subscription,
    timeout: Duration,
    observed: &mut Observed,
    predicate: impl Fn(&Observed) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while !predicate(observed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "observation timed out: rows={} evidence={:?} execution={:?}",
                observed.rows.len(),
                observed.evidence,
                observed.execution
            ));
        }
        let frame = match subscription.recv_timeout(remaining) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "observation timed out: rows={} evidence={:?} execution={:?}",
                    observed.rows.len(),
                    observed.evidence,
                    observed.execution
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "observation disconnected: rows={} evidence={:?} execution={:?}",
                    observed.rows.len(),
                    observed.evidence,
                    observed.execution
                ));
            }
        };
        observed.apply(frame);
    }
    Ok(())
}
