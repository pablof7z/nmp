//! One acceptance corpus for engine-free NIP-19 reference semantics.
//!
//! The direct Rust/FFI parity harness and both native SDK suites consume the
//! exact same JSON bytes. Platform tests normalize their public values into
//! this schema; no platform keeps an alternate table of expected targets or
//! demands.

use std::collections::BTreeMap;

use serde::Deserialize;

const REFERENCE_FIXTURE_JSON: &str = include_str!("../../../fixtures/reference-plans.json");

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReferenceFixtureCorpus {
    pub schema: u16,
    pub cases: Vec<ReferenceFixture>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReferenceFixture {
    pub name: String,
    pub input: String,
    pub outcome: ReferenceFixtureOutcome,
    pub target: Option<NormalizedReferenceTarget>,
    pub plan: Option<NormalizedReferencePlan>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceFixtureOutcome {
    Public,
    SecretKey,
    Malformed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NormalizedReferenceTarget {
    pub kind: String,
    pub key: String,
    pub pubkey: Option<String>,
    pub id: Option<String>,
    pub author_hint: Option<String>,
    pub kind_hint: Option<u16>,
    pub address_kind: Option<u16>,
    pub author: Option<String>,
    pub identifier: Option<String>,
    pub relay_hints: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NormalizedReferencePlan {
    pub target_key: String,
    pub canonical: NormalizedDemand,
    pub helpers: Vec<NormalizedDemand>,
    pub discarded_relay_hints: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NormalizedDemand {
    pub selection: NormalizedFilter,
    pub source: NormalizedSource,
    pub access: String,
    pub cache: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NormalizedFilter {
    pub kinds: Vec<u16>,
    pub authors: Vec<String>,
    pub ids: Vec<String>,
    pub tags: BTreeMap<String, Vec<String>>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NormalizedSource {
    pub kind: String,
    pub relays: Vec<String>,
}

#[must_use]
pub fn reference_fixture_json() -> &'static str {
    REFERENCE_FIXTURE_JSON
}

#[must_use]
pub fn reference_fixtures() -> ReferenceFixtureCorpus {
    serde_json::from_str(REFERENCE_FIXTURE_JSON)
        .expect("shared NIP-19 reference fixtures must match their versioned schema")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn shared_corpus_is_versioned_unique_and_covers_every_required_outcome() {
        let corpus = reference_fixtures();
        assert_eq!(corpus.schema, 1);

        let names = corpus
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names.len(),
            corpus.cases.len(),
            "fixture names must be unique"
        );

        let public_kinds = corpus
            .cases
            .iter()
            .filter_map(|case| case.target.as_ref().map(|target| target.kind.as_str()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            public_kinds,
            BTreeSet::from(["address", "event", "profile"])
        );
        assert!(corpus
            .cases
            .iter()
            .any(|case| case.outcome == ReferenceFixtureOutcome::SecretKey));
        assert!(corpus
            .cases
            .iter()
            .any(|case| case.outcome == ReferenceFixtureOutcome::Malformed));
    }

    #[test]
    fn only_public_entities_carry_actionable_expectations() {
        for case in reference_fixtures().cases {
            match case.outcome {
                ReferenceFixtureOutcome::Public => {
                    let target = case.target.expect("public fixture must carry a target");
                    let plan = case.plan.expect("public fixture must carry a demand plan");
                    assert_eq!(target.key, plan.target_key);
                }
                ReferenceFixtureOutcome::SecretKey | ReferenceFixtureOutcome::Malformed => {
                    assert!(
                        case.target.is_none(),
                        "{} unexpectedly has a target",
                        case.name
                    );
                    assert!(case.plan.is_none(), "{} unexpectedly has a plan", case.name);
                }
            }
        }
    }
}
