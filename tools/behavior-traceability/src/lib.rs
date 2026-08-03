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
    use std::collections::{BTreeMap, BTreeSet};

    /// The real governed corpus's `specified`/`known-violation` issue
    /// references, as of this checkpoint: `<issue number> -> <its real
    /// state>`. This self-test does not hold a GitHub token (the head-built
    /// checker never does), so it hard-codes the exact live set rather than
    /// fetching it — mirroring the *shape* of the trusted CI snapshot
    /// (`IssueSnapshot`) instead of refusing every lookup outright. A
    /// governed scenario that names an issue this map does not carry is a
    /// real traceability bug this test must catch, not silently pass;
    /// `verify_exact` below catches the opposite drift, a stale entry
    /// nothing governs any more.
    ///
    /// Update this map in the same change that adds, removes, or closes a
    /// governed `nmp:issue` reference — exactly as a real PR must keep the
    /// CI-built trusted snapshot in step with the corpus it validates.
    ///
    /// One entry as of #1214: `QUERIES-COMPOSED-027` names #1215, the open
    /// gap that no runtime test can assert the native live query keeps its
    /// branch storage private and unforgeable.
    struct KnownLiveIssues(BTreeMap<u64, IssueState>);

    impl KnownLiveIssues {
        fn current() -> Self {
            Self(BTreeMap::from([(1215, IssueState::Open)]))
        }
    }

    impl IssueLookup for KnownLiveIssues {
        fn state(&self, issue: u64) -> Result<IssueState, TraceError> {
            self.0.get(&issue).copied().ok_or_else(|| {
                TraceError(format!(
                    "self-test fixture KnownLiveIssues does not carry issue #{issue}; add its \
                     real state to tools/behavior-traceability/src/lib.rs in the same change \
                     that adds this governed `nmp:issue` reference"
                ))
            })
        }

        fn verify_exact(&self, required: &BTreeSet<u64>) -> Result<(), TraceError> {
            let supplied: BTreeSet<_> = self.0.keys().copied().collect();
            if supplied != *required {
                return Err(TraceError(format!(
                    "self-test fixture KnownLiveIssues does not exactly match governed \
                     metadata: expected {required:?}, got {supplied:?}"
                )));
            }
            Ok(())
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
        crate::validate::validate(root, &corpus, &KnownLiveIssues::current()).unwrap();
    }

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

    /// Falsifier 1 (#1197): a `specified` scenario naming a real open issue
    /// passes the lane. Falsifier 4: emptying the fixture again makes it red.
    #[test]
    fn specified_scenario_naming_a_known_open_issue_validates() {
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
             # nmp:issue=#1189\n  \
             Scenario: specified behaviour, honestly unproven\n    Given truth\n",
        );
        let corpus = crate::corpus::load(&features).unwrap();

        let known = KnownLiveIssues(BTreeMap::from([(1189, IssueState::Open)]));
        crate::validate::validate(temp.path(), &corpus, &known).unwrap();

        // Falsifier 4: emptying the fixture again must make this red.
        let emptied = KnownLiveIssues::current();
        let error = crate::validate::validate(temp.path(), &corpus, &emptied).unwrap_err();
        assert!(
            error.0.contains("does not carry issue #1189"),
            "unexpected error: {error}"
        );
    }

    /// Falsifier 2 (#1197): a `known-violation` scenario naming a real open
    /// issue passes the lane.
    #[test]
    fn known_violation_scenario_naming_a_known_open_issue_validates() {
        let temp = minimal_cargo_root();
        let features = temp.path().join("features");
        std::fs::create_dir_all(&features).unwrap();
        write_feature(
            &features,
            "violation.feature",
            "Feature: falsifier two\n  \
             # nmp:id=FALSIFIER-VIOLATION-001\n  \
             # nmp:status=known-violation\n  \
             # nmp:issue=#1190\n  \
             Scenario: behaviour the code contradicts today\n    Given truth\n",
        );
        let corpus = crate::corpus::load(&features).unwrap();

        let known = KnownLiveIssues(BTreeMap::from([(1190, IssueState::Open)]));
        crate::validate::validate(temp.path(), &corpus, &known).unwrap();

        let emptied = KnownLiveIssues::current();
        let error = crate::validate::validate(temp.path(), &corpus, &emptied).unwrap_err();
        assert!(
            error.0.contains("does not carry issue #1190"),
            "unexpected error: {error}"
        );
    }

    /// Falsifier 3 (#1197): a scenario naming an issue that does not exist
    /// still fails, distinguishably from a fixture error — proven against
    /// the real production `IssueLookup` (`IssueSnapshot`), not the
    /// self-test double, since that is the mechanism a nonexistent issue
    /// actually goes through in CI.
    #[test]
    fn nonexistent_issue_fails_by_name_not_as_a_fixture_error() {
        let temp = tempfile::tempdir().unwrap();
        let issues_path = temp.path().join("issues.tsv");
        std::fs::write(&issues_path, "nmp-behavior-issue-snapshot-v1\n1189\topen\n").unwrap();
        let snapshot = crate::IssueSnapshot::from_path(&issues_path).unwrap();

        let error = snapshot.state(9999).unwrap_err();
        assert!(error.0.contains("#9999"), "{error}");
        assert!(error.0.contains("missing or unreadable"), "{error}");
        assert!(!error.0.to_lowercase().contains("fixture"), "{error}");
    }
}
