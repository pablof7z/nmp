//! Repository-owned behavioral traceability.
//!
//! This crate deliberately does not depend on the transitional `nmp-bdd`
//! mechanism runner. It parses the canonical `features/` corpus through the
//! official Gherkin 0.14 AST and validates status/evidence/issue truth. The
//! tool is a detached, independently locked workspace so product-workspace
//! dependency edits cannot change the checker that judges them.

mod corpus;
mod evidence;
#[cfg(test)]
mod evidence_tests;
mod git;
mod issues;
mod model;
mod validate;

use std::path::Path;

pub use evidence::{EvidenceKind, EvidenceLocator};
pub use issues::{IssueLookup, IssueSnapshot, IssueState};
pub use model::{Gap, Metadata, ScenarioRecord, Status, TraceError};

/// Validate the checked-out canonical corpus against explicit git revisions.
///
/// The explicit head must be the checkout's actual `HEAD`; otherwise evidence
/// resolution would mix two trees and the check fails closed.
pub fn validate_repository(
    root: &Path,
    base: &str,
    head: &str,
    issues: &dyn IssueLookup,
) -> Result<(), TraceError> {
    git::validate_diff(root, base, head, issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real governed corpus's `specified`/`known-violation` issue
    /// references, as of this checkpoint. This self-test does not hold a
    /// GitHub token (the head-built checker never does), so it hard-codes
    /// the exact live set rather than fetching it; a governed scenario that
    /// names any other issue is a real traceability bug this test must
    /// catch, not silently pass.
    ///
    /// One entry as of #1214: `QUERIES-COMPOSED-027` names #1215, the open
    /// gap that no runtime test can assert the native live query's branch
    /// storage stays private and unforgeable.
    struct KnownLiveIssues;

    impl IssueLookup for KnownLiveIssues {
        fn state(&self, issue: u64) -> Result<IssueState, TraceError> {
            match issue {
                1215 => Ok(IssueState::Open),
                _ => Err(TraceError(format!(
                    "real fixture unexpectedly requested untracked issue #{issue}"
                ))),
            }
        }
    }

    #[test]
    fn repositorys_governed_slice_resolves_to_real_owner_and_ci_lane() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let corpus = crate::corpus::load(&root.join("features")).unwrap();
        crate::validate::validate(root, &corpus, &KnownLiveIssues).unwrap();
    }
}
