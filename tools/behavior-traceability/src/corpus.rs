use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use gherkin::{Background, Examples, Feature, GherkinEnv, Rule, Scenario, Step};

use crate::model::{Gap, Metadata, ScenarioRecord, Status, TraceError};

#[derive(Debug)]
pub(crate) struct Corpus {
    pub records: Vec<ScenarioRecord>,
    pub governed_files: BTreeSet<PathBuf>,
}

#[derive(Default)]
struct RawMetadata {
    values: BTreeMap<String, Vec<String>>,
    source_offsets: BTreeSet<usize>,
}

impl RawMetadata {
    fn is_present(&self) -> bool {
        !self.values.is_empty()
    }

    fn one(&self, key: &str) -> Result<Option<&str>, TraceError> {
        match self.values.get(key).map(Vec::as_slice) {
            None => Ok(None),
            Some([value]) => Ok(Some(value)),
            Some(values) => Err(TraceError(format!(
                "metadata key `nmp:{key}` must occur exactly once, found {}",
                values.len()
            ))),
        }
    }

    fn parse(self) -> Result<Option<Metadata>, TraceError> {
        if !self.is_present() {
            return Ok(None);
        }
        let allowed = BTreeSet::from(["id", "status", "evidence", "falsifier", "gap", "issue"]);
        if let Some(key) = self
            .values
            .keys()
            .find(|key| !allowed.contains(key.as_str()))
        {
            return Err(TraceError(format!("unknown metadata key `nmp:{key}`")));
        }

        let id = self
            .one("id")?
            .ok_or_else(|| TraceError("governed scenario is missing `nmp:id`".into()))?
            .to_owned();
        let status_value = self
            .one("status")?
            .ok_or_else(|| TraceError("governed scenario is missing `nmp:status`".into()))?;
        let status = Status::parse(status_value).ok_or_else(|| {
            TraceError(format!(
                "invalid `nmp:status={status_value}`; expected specified, built, or known-violation"
            ))
        })?;
        let evidence = self.values.get("evidence").cloned().unwrap_or_default();
        let falsifier = self.one("falsifier")?.map(str::to_owned);
        let gap = self
            .one("gap")?
            .map(|value| {
                Gap::parse(value).ok_or_else(|| {
                    TraceError(format!(
                        "invalid `nmp:gap={value}`; expected implementation, evidence, fixture, or platform"
                    ))
                })
            })
            .transpose()?;
        let issue = self
            .one("issue")?
            .map(|value| {
                value
                    .strip_prefix('#')
                    .unwrap_or(value)
                    .parse::<u64>()
                    .map_err(|_| TraceError(format!("invalid `nmp:issue={value}`")))
            })
            .transpose()?;
        Ok(Some(Metadata {
            id,
            status,
            evidence,
            falsifier,
            gap,
            issue,
        }))
    }
}

pub(crate) fn load(features_dir: &Path) -> Result<Corpus, TraceError> {
    let root_metadata = fs::symlink_metadata(features_dir).map_err(|error| {
        TraceError(format!(
            "canonical feature root is unreadable: {}: {error}",
            features_dir.display()
        ))
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(TraceError(format!(
            "canonical feature root must be a repository-owned directory, not a symlink: {}",
            features_dir.display()
        )));
    }
    let canonical_root = fs::canonicalize(features_dir).map_err(|error| {
        TraceError(format!(
            "cannot canonicalize feature root {}: {error}",
            features_dir.display()
        ))
    })?;
    let mut paths = Vec::new();
    collect_feature_paths(&canonical_root, features_dir, &mut paths)?;
    paths.sort();

    let mut records = Vec::new();
    let mut governed_files = BTreeSet::new();
    for path in paths {
        let source = fs::read_to_string(&path).map_err(|error| {
            TraceError(format!("cannot read feature {}: {error}", path.display()))
        })?;
        let feature = Feature::parse_path(&path, GherkinEnv::default()).map_err(|error| {
            TraceError(format!("cannot parse feature {}: {error}", path.display()))
        })?;
        let (mut file_records, attached_offsets) = records_for_feature(&path, &source, &feature)?;
        let all_offsets = metadata_source_offsets(&source);
        if let Some(offset) = all_offsets.difference(&attached_offsets).next() {
            let line = source[..*offset].lines().count() + 1;
            return Err(TraceError(format!(
                "{}:{line}: orphan or misplaced `# nmp:*` metadata is not immediately attached to a Scenario or Scenario Outline",
                path.display()
            )));
        }
        if file_records
            .iter()
            .any(|record| record.raw_metadata_present)
        {
            governed_files.insert(path.clone());
        }
        records.append(&mut file_records);
    }
    Ok(Corpus {
        records,
        governed_files,
    })
}

fn collect_feature_paths(
    canonical_root: &Path,
    dir: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), TraceError> {
    for entry in fs::read_dir(dir)
        .map_err(|error| TraceError(format!("cannot enumerate {}: {error}", dir.display())))?
    {
        let entry = entry.map_err(|error| {
            TraceError(format!(
                "cannot enumerate entry under {}: {error}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| TraceError(format!("cannot inspect {}: {error}", path.display())))?;
        if file_type.is_symlink() {
            return Err(TraceError(format!(
                "feature corpus path {} is symlink-backed instead of repository-owned",
                path.display()
            )));
        }
        let canonical_path = fs::canonicalize(&path).map_err(|error| {
            TraceError(format!(
                "cannot canonicalize feature corpus path {}: {error}",
                path.display()
            ))
        })?;
        if !canonical_path.starts_with(canonical_root) {
            return Err(TraceError(format!(
                "feature corpus path {} escapes canonical feature root {}",
                path.display(),
                canonical_root.display()
            )));
        }
        if file_type.is_dir() {
            collect_feature_paths(canonical_root, &path, paths)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "feature") {
            paths.push(path);
        } else if path.extension().is_some_and(|ext| ext == "feature") {
            return Err(TraceError(format!(
                "feature corpus path {} is not a repository-owned regular file",
                path.display()
            )));
        }
    }
    Ok(())
}

fn records_for_feature(
    path: &Path,
    source: &str,
    feature: &Feature,
) -> Result<(Vec<ScenarioRecord>, BTreeSet<usize>), TraceError> {
    let mut records = Vec::new();
    let mut attached_offsets = BTreeSet::new();
    for scenario in &feature.scenarios {
        let (scenario_record, offsets) = record(path, source, feature, None, scenario)?;
        records.push(scenario_record);
        attached_offsets.extend(offsets);
    }
    for rule in &feature.rules {
        for scenario in &rule.scenarios {
            let (scenario_record, offsets) = record(path, source, feature, Some(rule), scenario)?;
            records.push(scenario_record);
            attached_offsets.extend(offsets);
        }
    }
    Ok((records, attached_offsets))
}

fn record(
    path: &Path,
    source: &str,
    feature: &Feature,
    rule: Option<&Rule>,
    scenario: &Scenario,
) -> Result<(ScenarioRecord, BTreeSet<usize>), TraceError> {
    let raw = metadata_before_span(source, scenario.span.start)?;
    let raw_metadata_present = raw.is_present();
    let attached_offsets = raw.source_offsets.clone();
    let metadata = raw.parse().map_err(|error| {
        TraceError(format!(
            "{}:{} (`{}`): {}",
            path.display(),
            scenario.position.line,
            scenario.name,
            error
        ))
    })?;
    let mut effective_tags = feature.tags.clone();
    if let Some(rule) = rule {
        effective_tags.extend(rule.tags.iter().cloned());
    }
    effective_tags.extend(scenario.tags.iter().cloned());
    for examples in &scenario.examples {
        effective_tags.extend(examples.tags.iter().cloned());
    }

    let rule_name = rule.map(|rule| rule.name.as_str()).unwrap_or("");
    let rule_description = rule.and_then(|rule| rule.description.as_deref());
    let rule_keyword = rule.map(|rule| rule.keyword.as_str()).unwrap_or("");
    let rule_background = semantic_background(rule.and_then(|rule| rule.background.as_ref()));
    let fingerprint = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        feature.keyword,
        feature.name,
        feature.description,
        semantic_background(feature.background.as_ref()),
        rule_keyword,
        rule_name,
        rule_description,
        rule_background,
        semantic_scenario(scenario),
        effective_tags,
    );
    Ok((
        ScenarioRecord {
            file: path.to_path_buf(),
            line: scenario.position.line,
            name: scenario.name.clone(),
            effective_tags,
            metadata,
            raw_metadata_present,
            fingerprint,
        },
        attached_offsets,
    ))
}

fn semantic_background(background: Option<&Background>) -> Option<String> {
    background.map(|background| {
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            background.keyword,
            background.name,
            background.description,
            semantic_steps(&background.steps)
        )
    })
}

fn semantic_scenario(scenario: &Scenario) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        scenario.keyword,
        scenario.name,
        scenario.description,
        semantic_steps(&scenario.steps),
        scenario
            .examples
            .iter()
            .map(semantic_examples)
            .collect::<Vec<_>>()
    )
}

fn semantic_examples(examples: &Examples) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        examples.keyword,
        examples.name,
        examples.description,
        examples.table.as_ref().map(|table| &table.rows),
        examples.tags
    )
}

fn semantic_steps(steps: &[Step]) -> Vec<String> {
    steps
        .iter()
        .map(|step| {
            format!(
                "{:?}|{:?}|{:?}|{:?}|{:?}",
                step.keyword,
                step.ty,
                step.value,
                step.docstring,
                step.table.as_ref().map(|table| &table.rows)
            )
        })
        .collect()
}

fn metadata_before_span(source: &str, scenario_start: usize) -> Result<RawMetadata, TraceError> {
    // gherkin 0.14 starts Scenario.span at the directive, after its leading
    // tags. Walk only the source interval immediately before that byte so
    // metadata may cross tag/comment/blank trivia but never a Feature, Rule,
    // description, step, Examples block, or prior scenario.
    let prefix = source.get(..scenario_start).ok_or_else(|| {
        TraceError(format!(
            "Gherkin scenario span starts outside its source at byte {scenario_start}"
        ))
    })?;
    let lines = source_lines(prefix);
    let mut index = lines.len();
    let mut metadata_lines = Vec::new();
    while index > 0 {
        index -= 1;
        let (offset, line) = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('@') {
            continue;
        }
        if trimmed.starts_with('#') {
            if let Some(value) = metadata_line_value(line) {
                metadata_lines.push((offset, value.trim().to_owned()));
            }
            continue;
        }
        break;
    }
    metadata_lines.reverse();

    let mut raw = RawMetadata::default();
    for (offset, line) in metadata_lines {
        let (key, value) = line.split_once('=').ok_or_else(|| {
            TraceError(format!(
                "metadata line `# nmp:{line}` must use `# nmp:key=value`"
            ))
        })?;
        let key = key.trim().to_owned();
        let value = value.trim().to_owned();
        if key.is_empty() || value.is_empty() {
            return Err(TraceError(format!(
                "metadata line `# nmp:{line}` has an empty key or value"
            )));
        }
        raw.source_offsets.insert(offset);
        raw.values.entry(key).or_default().push(value);
    }
    Ok(raw)
}

fn source_lines(source: &str) -> Vec<(usize, &str)> {
    let mut offset = 0;
    let mut lines = Vec::new();
    for line in source.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        lines.push((offset, content));
        offset += line.len();
    }
    lines
}

fn metadata_source_offsets(source: &str) -> BTreeSet<usize> {
    source_lines(source)
        .into_iter()
        .filter_map(|(offset, line)| metadata_line_value(line).map(|_| offset))
        .collect()
}

fn metadata_line_value(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix('#')?
        .trim_start()
        .strip_prefix("nmp:")
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    fn load_one(source: &str) -> Result<Corpus, TraceError> {
        let temp = tempdir().unwrap();
        let path = temp.path().join("sample.feature");
        fs::write(path, source).unwrap();
        load(temp.path())
    }

    #[test]
    fn real_ast_covers_feature_rule_outline_and_effective_tags() {
        let corpus = load_one(
            r#"
@requires-network
Feature: AST truth
  @wip
  Rule: inherited lifecycle
    # ordinary comment is trivia
    # nmp:id=AST-RULE-001
    # nmp:status=specified
    # nmp:gap=fixture
    # nmp:issue=#12
    @scenario-tag
    # ignored comment between tags
    Scenario Outline: parsed outline
      Given a <value>
      Examples:
        | value |
        | one   |
"#,
        )
        .unwrap();
        assert_eq!(corpus.records.len(), 1);
        let record = &corpus.records[0];
        assert!(record.effective_tags.contains(&"requires-network".into()));
        assert!(record.effective_tags.contains(&"wip".into()));
        assert!(record.effective_tags.contains(&"scenario-tag".into()));
        assert_eq!(record.metadata.as_ref().unwrap().id, "AST-RULE-001");
    }

    #[test]
    fn scenario_span_attachment_crosses_only_leading_tag_comment_and_blank_trivia() {
        let corpus = load_one(
            r#"
Feature: span truth
  Rule: exact interval
    # nmp:id=SPAN-INTERVAL-001
    # nmp:status=specified
    # nmp:gap=fixture
    # nmp:issue=#12

    # ordinary ignored comment
    @scenario-tag
    Scenario Outline: attached after leading tags
      Given a <value>
      Examples:
        | value |
        | one   |
"#,
        )
        .unwrap();
        assert_eq!(
            corpus.records[0].metadata.as_ref().unwrap().id,
            "SPAN-INTERVAL-001"
        );

        for directive in [
            "Feature: boundary\n",
            "Feature: boundary\n  Rule: separator\n",
            "Feature: boundary\n  Scenario: previous\n    Given a step\n",
            "Feature: boundary\n  free-form description\n",
        ] {
            let source = format!(
                "# nmp:id=ORPHAN-BOUNDARY-001\n# nmp:status=specified\n# nmp:gap=fixture\n# nmp:issue=#12\n{directive}  Scenario: target\n    Given truth\n"
            );
            let error = load_one(&source).unwrap_err();
            assert!(
                error.0.contains("orphan or misplaced"),
                "directive `{directive}` unexpectedly attached metadata: {error}"
            );
        }
    }

    #[test]
    fn metadata_comment_whitespace_has_one_lexical_boundary() {
        let corpus = load_one(
            "Feature: whitespace\n  #    nmp:id=WHITESPACE-METADATA-001\n\t#\tnmp:status=specified\n  #  nmp:gap=fixture\n  #     nmp:issue=#12\n  Scenario: governed\n    Given truth\n",
        )
        .unwrap();
        assert_eq!(
            corpus.records[0].metadata.as_ref().unwrap().id,
            "WHITESPACE-METADATA-001"
        );
        assert_eq!(corpus.governed_files.len(), 1);
    }

    #[test]
    fn examples_tags_contribute_to_effective_lifecycle_and_acceptance_tags() {
        let corpus = load_one(
            r#"
Feature: examples tags
  # nmp:id=EXAMPLES-TAGS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::facade_proof
  # nmp:falsifier=remove the facade proof
  Scenario Outline: inherited from examples
    Given <value>
    @requires-network @acceptance
    Examples:
      | value |
      | one   |
"#,
        )
        .unwrap();
        let tags = &corpus.records[0].effective_tags;
        assert!(tags.contains(&"requires-network".into()));
        assert!(tags.contains(&"acceptance".into()));
        assert!(crate::validate::validate_tags(&corpus.records[0])
            .unwrap_err()
            .0
            .contains("@requires-network"));
    }

    #[test]
    fn metadata_before_feature_rule_step_description_or_examples_is_orphaned() {
        for source in [
            "# nmp:id=ORPHAN-FEATURE-001\nFeature: orphan\n  Scenario: target\n    Given truth\n",
            "Feature: orphan\n  # nmp:id=ORPHAN-RULE-001\n  Rule: boundary\n    Scenario: target\n      Given truth\n",
            "Feature: orphan\n  Scenario: first\n    # nmp:id=ORPHAN-STEP-001\n    Given truth\n\n  Scenario: target\n    Given truth\n",
            "Feature: orphan\n  # nmp:id=ORPHAN-DESCRIPTION-001\n  descriptive boundary\n  Scenario: target\n    Given truth\n",
            "Feature: orphan\n  Scenario Outline: outline\n    Given <value>\n    # nmp:id=ORPHAN-EXAMPLES-001\n    Examples:\n      | value |\n      | one   |\n",
        ] {
            let error = load_one(source).unwrap_err();
            assert!(error.0.contains("orphan or misplaced"), "{error}");
        }
    }

    #[test]
    fn malformed_gherkin_fails_through_official_parser() {
        let error = load_one("Scenario: no containing feature\n  Given truth\n").unwrap_err();
        assert!(error.0.contains("cannot parse feature"));
    }

    #[test]
    fn repeated_evidence_is_ordered_and_singletons_reject_duplicates() {
        let corpus = load_one(
            r#"
Feature: metadata
  # nmp:id=META-ORDER-001
  # nmp:status=built
  # nmp:evidence=rust:one::first
  # nmp:evidence=script:repository::scripts/proof.sh
  # nmp:falsifier=break it
  Scenario: ordered
    Given truth
"#,
        )
        .unwrap();
        assert_eq!(
            corpus.records[0].metadata.as_ref().unwrap().evidence,
            ["rust:one::first", "script:repository::scripts/proof.sh"]
        );

        let error = load_one(
            r#"
Feature: duplicate
  # nmp:id=META-DUP-001
  # nmp:id=META-DUP-002
  # nmp:status=specified
  Scenario: duplicate
    Given truth
"#,
        )
        .unwrap_err();
        assert!(error.0.contains("must occur exactly once"));
    }

    #[test]
    fn one_metadata_block_governs_the_whole_file() {
        let corpus = load_one(
            r#"
Feature: incremental governance
  # nmp:id=GOVERNED-FILE-001
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#12
  Scenario: governed
    Given truth

  Scenario: missing metadata
    Given more truth
"#,
        )
        .unwrap();
        assert_eq!(corpus.governed_files.len(), 1);
        assert!(corpus.records[0].metadata.is_some());
        assert!(corpus.records[1].metadata.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_backed_feature_is_not_repository_owned_corpus() {
        let repository = tempdir().unwrap();
        let external = tempdir().unwrap();
        let features = repository.path().join("features");
        fs::create_dir(&features).unwrap();
        let external_feature = external.path().join("borrowed.feature");
        fs::write(
            &external_feature,
            "Feature: external\n  Scenario: borrowed\n    Given external truth\n",
        )
        .unwrap();
        symlink(&external_feature, features.join("borrowed.feature")).unwrap();

        assert!(load(&features).unwrap_err().0.contains("symlink"));
    }

    #[test]
    fn substantive_source_stops_metadata_attachment() {
        let error = load_one(
            r#"
Feature: source span attachment
  # nmp:id=WRONG-PLACE-001
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#12
  Background:
    Given shared setup

  Scenario: metadata does not cross the background
    Given truth
"#,
        )
        .unwrap_err();
        assert!(error.0.contains("orphan or misplaced"));
    }

    #[test]
    fn incomplete_and_invalid_metadata_fail_at_the_ast_scenario() {
        let missing_id = load_one(
            r#"
Feature: missing
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#12
  Scenario: no id
    Given truth
"#,
        )
        .unwrap_err();
        assert!(missing_id.0.contains("missing `nmp:id`"));

        let invalid_status = load_one(
            r#"
Feature: invalid
  # nmp:id=INVALID-STATUS-001
  # nmp:status=almost-built
  Scenario: bad status
    Given truth
"#,
        )
        .unwrap_err();
        assert!(invalid_status.0.contains("invalid `nmp:status"));
    }
}
