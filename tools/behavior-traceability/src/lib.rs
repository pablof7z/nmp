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

    struct NoIssues;

    impl IssueLookup for NoIssues {
        fn state(&self, issue: u64) -> Result<IssueState, TraceError> {
            Err(TraceError(format!(
                "real built-only fixture unexpectedly requested issue #{issue}"
            )))
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
        crate::validate::validate(root, &corpus, &NoIssues).unwrap();
    }
}
