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

    fn write_feature(dir: &Path, name: &str, source: &str) {
        std::fs::write(dir.join(name), source).unwrap();
    }

    /// `validate::validate` always constructs an `EvidenceResolver`, which
    /// requires its root to be a real (if minimal) Cargo workspace with a
    /// readable, repository-owned `.github/workflows` directory — `new`
    /// loads workflows unconditionally, before any locator is resolved, so
    /// the directory must exist even though none of these falsifier
    /// scenarios cite workflow evidence. An empty no-op package plus an
    /// empty workflow directory is enough: it keeps these tests fast and
    /// independent of the product workspace.
    fn minimal_cargo_root() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"scratch\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(scratch.join("src")).unwrap();
        std::fs::write(
            scratch.join("Cargo.toml"),
            "[package]\nname = \"scratch\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n",
        )
        .unwrap();
        std::fs::write(scratch.join("src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(temp.path().join(".github/workflows")).unwrap();
        temp
    }

    fn issue_snapshot(records: &str) -> IssueSnapshot {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("issues.tsv");
        std::fs::write(&path, format!("nmp-behavior-issue-snapshot-v1\n{records}")).unwrap();
        IssueSnapshot::from_path(&path).unwrap()
    }

    /// A `specified` scenario accepts an injected open issue. An incomplete
    /// production-format snapshot makes the same scenario red.
    #[test]
    fn specified_scenario_naming_an_injected_open_issue_validates() {
        let temp = minimal_cargo_root();
        let features = temp.path().join("features");
        std::fs::create_dir_all(&features).unwrap();
        write_feature(
            &features,
            "specified.feature",
            "Feature: falsifier one\n  \
             # nmp:id=FALSIFIER-OPEN-001\n  \
             # nmp:status=specified\n  \
             # nmp:gap=evidence\n  \
             # nmp:issue=#424201\n  \
             Scenario: specified behaviour, honestly unproven\n    Given truth\n",
        );
        let corpus = crate::corpus::load(&features).unwrap();

        let known = issue_snapshot("424201\topen\n");
        crate::validate::validate(temp.path(), &corpus, &known).unwrap();

        let emptied = issue_snapshot("");
        let error = crate::validate::validate(temp.path(), &corpus, &emptied).unwrap_err();
        assert!(
            error.0.contains("issue #424201 is missing or unreadable"),
            "unexpected error: {error}"
        );
    }

    /// A `known-violation` scenario uses the same injected snapshot boundary.
    #[test]
    fn known_violation_scenario_naming_an_injected_open_issue_validates() {
        let temp = minimal_cargo_root();
        let features = temp.path().join("features");
        std::fs::create_dir_all(&features).unwrap();
        write_feature(
            &features,
            "violation.feature",
            "Feature: falsifier two\n  \
             # nmp:id=FALSIFIER-VIOLATION-001\n  \
             # nmp:status=known-violation\n  \
             # nmp:issue=#424202\n  \
             Scenario: behaviour the code contradicts today\n    Given truth\n",
        );
        let corpus = crate::corpus::load(&features).unwrap();

        let known = issue_snapshot("424202\topen\n");
        crate::validate::validate(temp.path(), &corpus, &known).unwrap();

        let emptied = issue_snapshot("");
        let error = crate::validate::validate(temp.path(), &corpus, &emptied).unwrap_err();
        assert!(
            error.0.contains("issue #424202 is missing or unreadable"),
            "unexpected error: {error}"
        );
    }

    /// A scenario naming an issue absent from the injected snapshot fails by
    /// issue number through the same production `IssueSnapshot` used in CI.
    #[test]
    fn nonexistent_issue_fails_by_name_not_as_a_fixture_error() {
        let temp = tempfile::tempdir().unwrap();
        let issues_path = temp.path().join("issues.tsv");
        std::fs::write(
            &issues_path,
            "nmp-behavior-issue-snapshot-v1\n424201\topen\n",
        )
        .unwrap();
        let snapshot = crate::IssueSnapshot::from_path(&issues_path).unwrap();

        let error = snapshot.state(424299).unwrap_err();
        assert!(error.0.contains("#424299"), "{error}");
        assert!(error.0.contains("missing or unreadable"), "{error}");
        assert!(!error.0.to_lowercase().contains("fixture"), "{error}");
    }
}
