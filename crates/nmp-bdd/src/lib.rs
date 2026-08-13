//! `nmp-bdd` — the BDD acceptance layer (`docs/bdd/000-bdd-approach.md`).
//! Test-only: no production crate ever depends on this one. The real entry
//! point is `tests/bdd.rs` (`harness = false`); this `src/` tree exists only
//! so that binary can `use nmp_bdd::{...}` the `World` + step catalog.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub mod steps;

/// Conservatively identify feature files the transitional mechanism runner
/// must not execute.
///
/// The detached traceability tool owns Gherkin parsing and metadata validity.
/// This legacy runner needs only a fail-closed sentinel: any comment whose
/// payload starts with `nmp:` after arbitrary indentation and comment
/// whitespace removes the entire file from this mechanism suite. Invalid or
/// misplaced metadata is therefore skipped here and rejected by the
/// independent traceability lane instead of accidentally becoming executable
/// truth.
pub fn governed_feature_paths(features_dir: &Path) -> io::Result<BTreeSet<PathBuf>> {
    let mut pending = vec![features_dir.to_path_buf()];
    let mut governed = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "feature") {
                let source = fs::read_to_string(&path)?;
                if source.lines().any(has_metadata_sentinel) {
                    governed.insert(path.canonicalize()?);
                }
            }
        }
    }
    Ok(governed)
}

fn has_metadata_sentinel(line: &str) -> bool {
    line.trim_start()
        .strip_prefix('#')
        .is_some_and(|comment| comment.trim_start().starts_with("nmp:"))
}

/// Does this step sentence say the scenario crosses a process boundary?
///
/// `tests/bdd.rs` asks it of every step BEFORE the scenario runs, and puts a
/// world that answers yes on a retained on-disk path. An engine-owned
/// temporary Redb directory cannot be reopened after its store is dropped, so
/// "I reconstruct the engine from the same durable store" is only a genuine
/// restart when the retained path was chosen with that sentence in
/// mind -- and the store is chosen once, at start-up, before any `When`
/// exists to ask. #974 answered this with a `Given` that set a flag; reading
/// the scenario's own words means a `.feature` never has to name the harness's
/// storage engine to get the behaviour it already asked for in English.
#[must_use]
pub fn step_crosses_a_process_boundary(step: &str) -> bool {
    step.contains("reconstruct the engine") || step.contains("the process stops")
}
pub mod world;

pub use world::NmpWorld;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitional_runner_skips_the_whole_file_on_any_metadata_sentinel() {
        let temp = tempfile::tempdir().unwrap();
        let governed = temp.path().join("governed.feature");
        let incomplete = temp.path().join("incomplete.feature");
        let whitespace = temp.path().join("whitespace.feature");
        let legacy = temp.path().join("legacy.feature");
        fs::write(
            &governed,
            "Feature: governed\n  # nmp:id=RUNNER-SKIP-001\n  Scenario: one\n    Given truth\n\n  Scenario: two\n    Given truth\n",
        )
        .unwrap();
        fs::write(
            &whitespace,
            "Feature: whitespace\n\t#    nmp:id=RUNNER-SKIP-002\n  Scenario: one\n    Given truth\n\n  Scenario: two\n    Given truth\n",
        )
        .unwrap();
        fs::write(
            &incomplete,
            "Feature: invalid\n  # nmp:unknown=still-fail-closed\n  Scenario: one\n    Given truth\n",
        )
        .unwrap();
        fs::write(
            &legacy,
            "Feature: legacy\n  @wip\n  Scenario: old filter\n    Given truth\n",
        )
        .unwrap();

        let paths = governed_feature_paths(temp.path()).unwrap();
        assert_eq!(
            paths,
            BTreeSet::from([
                governed.canonicalize().unwrap(),
                incomplete.canonicalize().unwrap(),
                whitespace.canonicalize().unwrap()
            ])
        );
        assert!(!paths.contains(&legacy.canonicalize().unwrap()));
    }
}
