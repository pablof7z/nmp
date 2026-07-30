use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::corpus::Corpus;
use crate::evidence::{EvidenceKind, EvidenceLocator, EvidenceResolver};
use crate::issues::{IssueLookup, IssueState};
use crate::model::{at, Gap, ScenarioRecord, Status, TraceError};

pub(crate) fn validate(
    root: &Path,
    corpus: &Corpus,
    issues: &dyn IssueLookup,
) -> Result<(), TraceError> {
    if corpus.governed_files.is_empty() {
        return Err(TraceError(
            "traceability check is vacuous: no governed feature file exists".into(),
        ));
    }
    let resolver = EvidenceResolver::new(root)?;
    let mut ids: BTreeMap<&str, &ScenarioRecord> = BTreeMap::new();
    let mut required_issues = BTreeSet::new();
    for record in &corpus.records {
        let governed = corpus.governed_files.contains(&record.file);
        if !governed {
            continue;
        }
        let metadata = record.metadata.as_ref().ok_or_else(|| {
            at(
                record,
                "every scenario in a governed file needs complete nmp metadata",
            )
        })?;
        validate_id(record, &metadata.id)?;
        register_id(&mut ids, record)?;
        validate_tags(record)?;
        validate_shape(record)?;
        reject_exact_duplicate_evidence(record)?;

        let mut locators = Vec::new();
        for value in &metadata.evidence {
            let locator = EvidenceLocator::parse(value).map_err(|error| at(record, error.0))?;
            resolver
                .resolve(&locator)
                .map_err(|error| at(record, error.0))?;
            locators.push(locator);
        }
        validate_evidence_policy(record, &locators)?;
        validate_issue(record, issues)?;
        if let Some(issue) = metadata.issue {
            required_issues.insert(issue);
        }
    }
    issues.verify_exact(&required_issues)?;
    Ok(())
}

fn reject_exact_duplicate_evidence(record: &ScenarioRecord) -> Result<(), TraceError> {
    let mut exact = BTreeSet::new();
    for value in &record
        .metadata
        .as_ref()
        .expect("governed metadata")
        .evidence
    {
        if !exact.insert(value) {
            return Err(at(
                record,
                format!("exact duplicate `nmp:evidence={value}`"),
            ));
        }
    }
    Ok(())
}

fn register_id<'a>(
    ids: &mut BTreeMap<&'a str, &'a ScenarioRecord>,
    record: &'a ScenarioRecord,
) -> Result<(), TraceError> {
    let metadata = record.metadata.as_ref().expect("governed metadata");
    if let Some(first) = ids.insert(&metadata.id, record) {
        return Err(at(
            record,
            format!(
                "duplicate `nmp:id={}` first appears at {}:{}",
                metadata.id,
                first.file.display(),
                first.line
            ),
        ));
    }
    Ok(())
}

fn validate_evidence_policy(
    record: &ScenarioRecord,
    locators: &[EvidenceLocator],
) -> Result<(), TraceError> {
    let metadata = record.metadata.as_ref().expect("governed metadata");
    if metadata.status == Status::Built
        && locators
            .iter()
            .all(|locator| locator.kind == EvidenceKind::Live)
    {
        return Err(at(
            record,
            "live evidence may supplement but cannot be the sole correctness proof",
        ));
    }
    if record.effective_tags.iter().any(|tag| tag == "acceptance")
        && !locators.iter().any(EvidenceLocator::is_facade_proof)
    {
        return Err(at(
            record,
            "`@acceptance` requires at least one `rust:nmp` facade proof",
        ));
    }
    Ok(())
}

fn validate_issue(record: &ScenarioRecord, issues: &dyn IssueLookup) -> Result<(), TraceError> {
    let Some(issue) = record.metadata.as_ref().expect("governed metadata").issue else {
        return Ok(());
    };
    match issues.state(issue).map_err(|error| at(record, error.0))? {
        IssueState::Open => Ok(()),
        IssueState::Closed => Err(at(record, format!("referenced issue #{issue} is closed"))),
    }
}

fn validate_id(record: &ScenarioRecord, id: &str) -> Result<(), TraceError> {
    let Some((prefix, number)) = id.rsplit_once('-') else {
        return Err(at(record, format!("invalid stable `nmp:id={id}`")));
    };
    let valid_prefix = prefix.split('-').all(|word| {
        word.starts_with(|character: char| character.is_ascii_uppercase())
            && word
                .chars()
                .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
    });
    let valid_number =
        number.len() == 3 && number.chars().all(|character| character.is_ascii_digit());
    if !valid_prefix || !valid_number {
        return Err(at(
            record,
            format!(
                "invalid stable `nmp:id={id}`; expected uppercase domain words and a three-digit suffix"
            ),
        ));
    }
    Ok(())
}

pub(crate) fn validate_tags(record: &ScenarioRecord) -> Result<(), TraceError> {
    if let Some(tag) = record.effective_tags.iter().find(|tag| {
        tag.as_str() == "wip" || tag.as_str() == "designed" || tag.starts_with("requires-")
    }) {
        return Err(at(
            record,
            format!("governed scenario inherits forbidden lifecycle/capability tag `@{tag}`"),
        ));
    }
    Ok(())
}

fn validate_shape(record: &ScenarioRecord) -> Result<(), TraceError> {
    let metadata = record.metadata.as_ref().expect("governed metadata");
    let acceptance = record.effective_tags.iter().any(|tag| tag == "acceptance");
    match metadata.status {
        Status::Built => {
            if metadata.evidence.is_empty() {
                return Err(at(
                    record,
                    "built scenario needs at least one `nmp:evidence`",
                ));
            }
            if metadata
                .falsifier
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(at(
                    record,
                    "built scenario needs one non-empty `nmp:falsifier`",
                ));
            }
            if metadata.gap.is_some() || metadata.issue.is_some() {
                return Err(at(
                    record,
                    "built scenario cannot carry `nmp:gap` or `nmp:issue`",
                ));
            }
        }
        Status::Specified => {
            if metadata.gap.is_none() || metadata.issue.is_none() {
                return Err(at(
                    record,
                    "specified scenario needs typed `nmp:gap` and open `nmp:issue`",
                ));
            }
            if !metadata.evidence.is_empty() || metadata.falsifier.is_some() {
                return Err(at(
                    record,
                    "specified scenario cannot carry built evidence or falsifier",
                ));
            }
        }
        Status::KnownViolation => {
            if metadata.issue.is_none() {
                return Err(at(
                    record,
                    "known-violation scenario needs open `nmp:issue`",
                ));
            }
            if metadata.gap.is_some()
                || !metadata.evidence.is_empty()
                || metadata.falsifier.is_some()
            {
                return Err(at(
                    record,
                    "known-violation scenario carries only its open issue",
                ));
            }
        }
    }
    if acceptance && metadata.status != Status::Built {
        return Err(at(
            record,
            "`@acceptance` is valid only on a built scenario",
        ));
    }
    let _all_gap_variants_are_deliberate = [
        Gap::Implementation,
        Gap::Evidence,
        Gap::Fixture,
        Gap::Platform,
    ];
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::IssueState;
    use crate::model::{Metadata, ScenarioRecord};
    use std::path::PathBuf;

    struct Issues(Result<IssueState, TraceError>);

    impl IssueLookup for Issues {
        fn state(&self, _issue: u64) -> Result<IssueState, TraceError> {
            self.0.clone()
        }
    }

    fn record(status: Status) -> ScenarioRecord {
        ScenarioRecord {
            file: PathBuf::from("features/test.feature"),
            line: 3,
            name: "test".into(),
            effective_tags: Vec::new(),
            metadata: Some(Metadata {
                id: "TEST-CASE-001".into(),
                status,
                evidence: Vec::new(),
                falsifier: None,
                gap: None,
                issue: None,
            }),
            raw_metadata_present: true,
            fingerprint: String::new(),
        }
    }

    #[test]
    fn status_shapes_and_acceptance_are_strict() {
        let mut built = record(Status::Built);
        assert!(validate_shape(&built).unwrap_err().0.contains("evidence"));
        built.metadata.as_mut().unwrap().evidence = vec!["live:probe::job".into()];
        built.metadata.as_mut().unwrap().falsifier = Some("break it".into());
        built.effective_tags.push("acceptance".into());
        assert!(validate_shape(&built).is_ok());

        let mut specified = record(Status::Specified);
        specified.metadata.as_mut().unwrap().gap = Some(Gap::Evidence);
        specified.metadata.as_mut().unwrap().issue = Some(1);
        assert!(validate_shape(&specified).is_ok());
        specified.effective_tags.push("acceptance".into());
        assert!(validate_shape(&specified).is_err());

        let mut violation = record(Status::KnownViolation);
        violation.metadata.as_mut().unwrap().issue = Some(1);
        assert!(validate_shape(&violation).is_ok());
    }

    #[test]
    fn acceptance_requires_facade_proof_and_live_cannot_stand_alone() {
        let mut built = record(Status::Built);
        built.metadata.as_mut().unwrap().evidence = vec!["live:probe::job".into()];
        built.metadata.as_mut().unwrap().falsifier = Some("break it".into());
        let live = EvidenceLocator::parse("live:probe::job").unwrap();
        assert!(validate_evidence_policy(&built, &[live])
            .unwrap_err()
            .0
            .contains("sole correctness"));

        built.effective_tags.push("acceptance".into());
        let mechanism = EvidenceLocator::parse("rust:nmp-router::proof").unwrap();
        assert!(validate_evidence_policy(&built, &[mechanism])
            .unwrap_err()
            .0
            .contains("rust:nmp"));
        let facade = EvidenceLocator::parse("rust:nmp::proof").unwrap();
        assert!(validate_evidence_policy(&built, &[facade]).is_ok());
    }

    #[test]
    fn stable_id_shape_is_strict() {
        let scenario = record(Status::Specified);
        assert!(validate_id(&scenario, "lower-case-1").is_err());
        assert!(validate_id(&scenario, "ROUTING-001").is_ok());
        assert!(validate_id(&scenario, "ROUTING-01").is_err());
        assert!(validate_id(&scenario, "ROUTING--001").is_err());

        let first = record(Status::Specified);
        let second = record(Status::Specified);
        let mut ids = BTreeMap::new();
        register_id(&mut ids, &first).unwrap();
        assert!(register_id(&mut ids, &second)
            .unwrap_err()
            .0
            .contains("duplicate"));
    }

    #[test]
    fn exact_duplicate_evidence_is_rejected_without_collapsing_distinct_order() {
        let mut scenario = record(Status::Built);
        scenario.metadata.as_mut().unwrap().evidence = vec![
            "rust:nmp::first".into(),
            "rust:nmp::second".into(),
            "rust:nmp::first".into(),
        ];
        assert!(reject_exact_duplicate_evidence(&scenario)
            .unwrap_err()
            .0
            .contains("exact duplicate"));
        scenario.metadata.as_mut().unwrap().evidence.pop();
        assert!(reject_exact_duplicate_evidence(&scenario).is_ok());
    }

    #[test]
    fn inherited_lifecycle_tags_are_rejected() {
        let mut scenario = record(Status::Specified);
        scenario.effective_tags.push("requires-network".into());
        assert!(validate_tags(&scenario)
            .unwrap_err()
            .0
            .contains("forbidden"));
    }

    #[test]
    fn issue_lookup_can_fail_closed() {
        let mut scenario = record(Status::Specified);
        scenario.metadata.as_mut().unwrap().gap = Some(Gap::Evidence);
        scenario.metadata.as_mut().unwrap().issue = Some(4);
        let unreadable = Issues(Err(TraceError("unreadable".into())));
        assert!(validate_issue(&scenario, &unreadable)
            .unwrap_err()
            .0
            .contains("unreadable"));
        let closed = Issues(Ok(IssueState::Closed));
        assert!(validate_issue(&scenario, &closed)
            .unwrap_err()
            .0
            .contains("closed"));
        let open = Issues(Ok(IssueState::Open));
        assert!(validate_issue(&scenario, &open).is_ok());
    }

    #[test]
    fn zero_governed_files_cannot_pass_vacuously() {
        let corpus = Corpus {
            records: Vec::new(),
            governed_files: BTreeSet::new(),
        };
        let open = Issues(Ok(IssueState::Open));
        assert!(validate(Path::new("."), &corpus, &open)
            .unwrap_err()
            .0
            .contains("vacuous"));
    }
}
