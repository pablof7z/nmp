use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::corpus::{self, Corpus};
use crate::issues::IssueLookup;
use crate::model::TraceError;
use crate::validate;

pub(crate) fn validate_diff(
    root: &Path,
    base: &str,
    head: &str,
    issues: &dyn IssueLookup,
) -> Result<(), TraceError> {
    if base.trim().is_empty() || head.trim().is_empty() {
        return Err(TraceError(
            "traceability validation requires explicit non-empty base and head revisions".into(),
        ));
    }
    let base_sha = resolve(root, base, "base")?;
    let head_sha = resolve(root, head, "head")?;
    let checkout_head = resolve(root, "HEAD", "checked-out HEAD")?;
    if head_sha != checkout_head {
        return Err(TraceError(format!(
            "explicit head {head_sha} is not the checked-out HEAD {checkout_head}; refusing mixed-tree validation"
        )));
    }
    require_clean_checkout(root)?;

    let head_features = root.join("features");
    let head_corpus = corpus::load(&head_features)?;
    validate::validate(root, &head_corpus, issues)?;

    let base_tree = extract_features(root, &base_sha)?;
    let base_features = base_tree.path().join("features");
    let base_corpus = corpus::load(&base_features)?;
    reject_changed_legacy(&base_corpus, &base_features, &head_corpus, &head_features)
}

fn require_clean_checkout(root: &Path) -> Result<(), TraceError> {
    let output = command(
        root,
        "git",
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        return Err(TraceError(format!(
            "cannot verify checked-out head cleanliness: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if !output.stdout.is_empty() {
        return Err(TraceError(
            "checked-out repository is dirty; refusing mixed-tree validation".into(),
        ));
    }
    Ok(())
}

fn resolve(root: &Path, revision: &str, label: &str) -> Result<String, TraceError> {
    let expression = format!("{revision}^{{commit}}");
    let output = command(root, "git", &["rev-parse", "--verify", &expression])?;
    if !output.status.success() {
        return Err(TraceError(format!(
            "explicit {label} revision `{revision}` is missing or unreadable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn extract_features(root: &Path, revision: &str) -> Result<TempDir, TraceError> {
    let listing = command(
        root,
        "git",
        &["ls-tree", "-r", "--name-only", revision, "--", "features"],
    )?;
    if !listing.status.success() {
        return Err(TraceError(format!(
            "cannot enumerate base feature corpus at {revision}: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        )));
    }
    let paths: Vec<_> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|path| path.ends_with(".feature"))
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        return Err(TraceError(format!(
            "base revision {revision} has no readable canonical feature corpus"
        )));
    }

    let temp = tempfile::tempdir().map_err(|error| {
        TraceError(format!(
            "cannot create base-tree scratch directory: {error}"
        ))
    })?;
    for relative in paths {
        let spec = format!("{revision}:{relative}");
        let blob = command(root, "git", &["show", &spec])?;
        if !blob.status.success() {
            return Err(TraceError(format!(
                "cannot read base feature `{relative}` at {revision}: {}",
                String::from_utf8_lossy(&blob.stderr).trim()
            )));
        }
        let destination = temp.path().join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                TraceError(format!(
                    "cannot create base feature directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        fs::write(&destination, blob.stdout).map_err(|error| {
            TraceError(format!(
                "cannot materialize base feature {}: {error}",
                destination.display()
            ))
        })?;
    }
    Ok(temp)
}

fn reject_changed_legacy(
    base: &Corpus,
    base_features: &Path,
    head: &Corpus,
    head_features: &Path,
) -> Result<(), TraceError> {
    let base_legacy_files = legacy_fingerprints_by_file(base, base_features)?;
    let head_files = fingerprints_by_file(head, head_features)?;
    for (file, fingerprints) in &head_files {
        let absolute = head_features.join(file);
        if head.governed_files.contains(&absolute) {
            continue;
        }
        match base_legacy_files.get(file) {
            Some(previous) if previous == fingerprints => {}
            Some(_) => {
                return Err(TraceError(format!(
                "changed behavior in ungoverned legacy file `{}` needs complete scenario metadata",
                Path::new("features").join(file).display()
            )))
            }
            None => {
                return Err(TraceError(format!(
                    "added or moved ungoverned behavior file `{}` needs complete scenario metadata",
                    Path::new("features").join(file).display()
                )))
            }
        }
    }
    for file in base_legacy_files.keys() {
        if !head_files.contains_key(file) {
            return Err(TraceError(format!(
                "deleted ungoverned behavior file `{}` needs complete scenario metadata before removal",
                Path::new("features").join(file).display()
            )));
        }
    }
    Ok(())
}

fn legacy_fingerprints_by_file(
    corpus: &Corpus,
    features_dir: &Path,
) -> Result<BTreeMap<PathBuf, Vec<String>>, TraceError> {
    fingerprints_by_file_where(corpus, features_dir, |record| record.metadata.is_none())
}

fn fingerprints_by_file(
    corpus: &Corpus,
    features_dir: &Path,
) -> Result<BTreeMap<PathBuf, Vec<String>>, TraceError> {
    fingerprints_by_file_where(corpus, features_dir, |_| true)
}

fn fingerprints_by_file_where(
    corpus: &Corpus,
    features_dir: &Path,
    include: impl Fn(&crate::model::ScenarioRecord) -> bool,
) -> Result<BTreeMap<PathBuf, Vec<String>>, TraceError> {
    let mut files: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for record in corpus.records.iter().filter(|record| include(record)) {
        let relative = record.file.strip_prefix(features_dir).map_err(|_| {
            TraceError(format!(
                "scenario path {} escapes canonical feature root {}",
                record.file.display(),
                features_dir.display()
            ))
        })?;
        files
            .entry(relative.to_path_buf())
            .or_default()
            .push(record.fingerprint.clone());
    }
    Ok(files)
}

fn command(root: &Path, program: &str, arguments: &[&str]) -> Result<Output, TraceError> {
    Command::new(program)
        .current_dir(root)
        .args(arguments)
        .output()
        .map_err(|error| {
            TraceError(format!(
                "cannot execute `{program} {}`: {error}",
                arguments.join(" ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn unchanged_legacy_is_allowed_but_changed_and_moved_legacy_fail() {
        let base_temp = tempdir().unwrap();
        let head_temp = tempdir().unwrap();
        let base_features = base_temp.path().join("features");
        let head_features = head_temp.path().join("features");
        fs::create_dir_all(base_features.join("domain")).unwrap();
        fs::create_dir_all(head_features.join("domain")).unwrap();
        let source = "Feature: legacy\n  Scenario: unchanged\n    Given truth\n";
        fs::write(base_features.join("domain/legacy.feature"), source).unwrap();
        fs::write(head_features.join("domain/legacy.feature"), source).unwrap();
        let base = corpus::load(&base_features).unwrap();
        let head = corpus::load(&head_features).unwrap();
        reject_changed_legacy(&base, &base_features, &head, &head_features).unwrap();

        fs::write(
            head_features.join("domain/legacy.feature"),
            "Feature: legacy\n  Scenario: changed\n    Given different truth\n",
        )
        .unwrap();
        let changed = corpus::load(&head_features).unwrap();
        assert!(
            reject_changed_legacy(&base, &base_features, &changed, &head_features)
                .unwrap_err()
                .0
                .contains("changed behavior")
        );

        fs::remove_file(head_features.join("domain/legacy.feature")).unwrap();
        fs::write(head_features.join("domain/moved.feature"), source).unwrap();
        let moved = corpus::load(&head_features).unwrap();
        assert!(
            reject_changed_legacy(&base, &base_features, &moved, &head_features)
                .unwrap_err()
                .0
                .contains("added or moved")
        );
    }

    #[test]
    fn a_governed_scenario_can_split_out_only_if_legacy_siblings_are_unchanged() {
        let base_temp = tempdir().unwrap();
        let head_temp = tempdir().unwrap();
        let base_features = base_temp.path().join("features");
        let head_features = head_temp.path().join("features");
        fs::create_dir_all(&base_features).unwrap();
        fs::create_dir_all(&head_features).unwrap();
        fs::write(
            base_features.join("mixed.feature"),
            "Feature: mixed\n  # nmp:id=MIXED-SPLIT-001\n  # nmp:status=specified\n  # nmp:gap=fixture\n  # nmp:issue=#12\n  Scenario: governed\n    Given governed truth\n\n  Scenario: legacy\n    Given legacy truth\n",
        )
        .unwrap();
        fs::write(
            head_features.join("mixed.feature"),
            "Feature: mixed\n  Scenario: legacy\n    Given legacy truth\n",
        )
        .unwrap();
        fs::write(
            head_features.join("governed.feature"),
            "Feature: governed\n  # nmp:id=MIXED-SPLIT-001\n  # nmp:status=specified\n  # nmp:gap=fixture\n  # nmp:issue=#12\n  Scenario: governed\n    Given governed truth\n",
        )
        .unwrap();

        let base = corpus::load(&base_features).unwrap();
        let head = corpus::load(&head_features).unwrap();
        reject_changed_legacy(&base, &base_features, &head, &head_features).unwrap();

        fs::write(
            head_features.join("mixed.feature"),
            "Feature: mixed\n  Scenario: legacy changed\n    Given different truth\n",
        )
        .unwrap();
        let changed = corpus::load(&head_features).unwrap();
        assert!(
            reject_changed_legacy(&base, &base_features, &changed, &head_features)
                .unwrap_err()
                .0
                .contains("changed behavior")
        );
    }

    #[test]
    fn deleted_legacy_behavior_fails_closed() {
        let base_temp = tempdir().unwrap();
        let head_temp = tempdir().unwrap();
        let base_features = base_temp.path().join("features");
        let head_features = head_temp.path().join("features");
        fs::create_dir_all(base_features.join("domain")).unwrap();
        fs::create_dir_all(head_features.join("domain")).unwrap();
        fs::write(
            base_features.join("domain/deleted.feature"),
            "Feature: legacy\n  Scenario: deleted\n    Given durable meaning\n",
        )
        .unwrap();

        let base = corpus::load(&base_features).unwrap();
        let head = corpus::load(&head_features).unwrap();
        assert!(
            reject_changed_legacy(&base, &base_features, &head, &head_features)
                .unwrap_err()
                .0
                .contains("deleted ungoverned behavior")
        );
    }

    #[test]
    fn empty_and_missing_revisions_fail_closed() {
        let root = tempdir().unwrap();
        assert!(resolve(root.path(), "", "base").is_err());
        assert!(resolve(root.path(), "missing", "base").is_err());
    }

    #[test]
    fn dirty_checkout_is_rejected_as_a_mixed_tree() {
        let root = tempdir().unwrap();
        command(root.path(), "git", &["init", "-q"]).unwrap();
        fs::write(root.path().join("tracked"), "base\n").unwrap();
        command(root.path(), "git", &["add", "tracked"]).unwrap();
        command(
            root.path(),
            "git",
            &[
                "-c",
                "user.name=Trace Test",
                "-c",
                "user.email=trace@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        )
        .unwrap();
        assert!(require_clean_checkout(root.path()).is_ok());
        fs::write(root.path().join("untracked"), "mixed\n").unwrap();
        assert!(require_clean_checkout(root.path())
            .unwrap_err()
            .0
            .contains("dirty"));
    }
}
