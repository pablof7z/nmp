use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::model::TraceError;

const HEADER: &str = "nmp-behavior-issue-snapshot-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueState {
    Open,
    Closed,
}

pub trait IssueLookup {
    fn state(&self, issue: u64) -> Result<IssueState, TraceError>;

    fn verify_exact(&self, _required: &BTreeSet<u64>) -> Result<(), TraceError> {
        Ok(())
    }
}

/// Credential-free issue-state data produced by the protected workflow.
///
/// The head-built checker never receives a GitHub token. The trusted workflow
/// resolves the deduplicated issue numbers to `open`/`closed` first and gives
/// this parser only the resulting narrow data file.
#[derive(Debug)]
pub struct IssueSnapshot {
    states: BTreeMap<u64, IssueState>,
}

impl IssueSnapshot {
    pub fn from_path(path: &Path) -> Result<Self, TraceError> {
        let source = fs::read_to_string(path).map_err(|error| {
            TraceError(format!(
                "issue-state snapshot is missing or unreadable at {}: {error}",
                path.display()
            ))
        })?;
        let mut lines = source.lines();
        if lines.next() != Some(HEADER) {
            return Err(TraceError(format!(
                "issue-state snapshot {} has an invalid or missing header",
                path.display()
            )));
        }
        let mut states = BTreeMap::new();
        for (index, line) in lines.enumerate() {
            if line.trim().is_empty() {
                return Err(TraceError(format!(
                    "issue-state snapshot {}:{} has an empty record",
                    path.display(),
                    index + 2
                )));
            }
            let Some((number, state)) = line.split_once('\t') else {
                return Err(TraceError(format!(
                    "issue-state snapshot {}:{} must be `<number>\\t<open|closed>`",
                    path.display(),
                    index + 2
                )));
            };
            let number = number.parse::<u64>().map_err(|_| {
                TraceError(format!(
                    "issue-state snapshot {}:{} has invalid issue number `{number}`",
                    path.display(),
                    index + 2
                ))
            })?;
            if number == 0 {
                return Err(TraceError(format!(
                    "issue-state snapshot {}:{} has invalid issue number `0`",
                    path.display(),
                    index + 2
                )));
            }
            let state = match state {
                "open" => IssueState::Open,
                "closed" => IssueState::Closed,
                other => {
                    return Err(TraceError(format!(
                        "issue-state snapshot {}:{} has unreadable state `{other}`",
                        path.display(),
                        index + 2
                    )))
                }
            };
            if states.insert(number, state).is_some() {
                return Err(TraceError(format!(
                    "issue-state snapshot {} repeats issue #{number}",
                    path.display()
                )));
            }
        }
        Ok(Self { states })
    }
}

impl IssueLookup for IssueSnapshot {
    fn state(&self, issue: u64) -> Result<IssueState, TraceError> {
        self.states.get(&issue).copied().ok_or_else(|| {
            TraceError(format!(
                "issue #{issue} is missing or unreadable in the trusted issue-state snapshot"
            ))
        })
    }

    fn verify_exact(&self, required: &BTreeSet<u64>) -> Result<(), TraceError> {
        let supplied: BTreeSet<_> = self.states.keys().copied().collect();
        if supplied != *required {
            return Err(TraceError(format!(
                "trusted issue-state snapshot numbers do not exactly match governed metadata: expected {required:?}, got {supplied:?}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn snapshot(source: &str) -> Result<IssueSnapshot, TraceError> {
        let temp = tempdir().unwrap();
        let path = temp.path().join("issues.tsv");
        fs::write(&path, source).unwrap();
        IssueSnapshot::from_path(&path)
    }

    #[test]
    fn trusted_snapshot_is_deduplicated_exact_and_fail_closed() {
        let open = snapshot(&format!("{HEADER}\n12\topen\n")).unwrap();
        assert_eq!(open.state(12).unwrap(), IssueState::Open);
        open.verify_exact(&BTreeSet::from([12])).unwrap();
        assert!(open
            .state(13)
            .unwrap_err()
            .0
            .contains("missing or unreadable"));
        assert!(open
            .verify_exact(&BTreeSet::from([13]))
            .unwrap_err()
            .0
            .contains("do not exactly match"));

        let incomplete = snapshot(&format!("{HEADER}\n")).unwrap();
        assert!(incomplete
            .verify_exact(&BTreeSet::from([12]))
            .unwrap_err()
            .0
            .contains("do not exactly match"));
        assert!(open
            .verify_exact(&BTreeSet::new())
            .unwrap_err()
            .0
            .contains("do not exactly match"));

        let closed = snapshot(&format!("{HEADER}\n12\tclosed\n")).unwrap();
        assert_eq!(closed.state(12).unwrap(), IssueState::Closed);
        assert!(snapshot(&format!("{HEADER}\n12\tunreadable\n"))
            .unwrap_err()
            .0
            .contains("unreadable state"));
        assert!(snapshot(&format!("{HEADER}\n12\topen\n12\topen\n"))
            .unwrap_err()
            .0
            .contains("repeats issue"));
        assert!(snapshot("wrong-header\n").is_err());
    }
}
