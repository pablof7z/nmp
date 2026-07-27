//! One acceptance corpus for the pure public NIP-19/NIP-21 locator codec.
//!
//! The direct Rust/FFI parity harness and both native SDK suites consume the
//! exact same JSON bytes. Platform tests normalize their public values into
//! this schema; no platform keeps an alternate table of expected locators.

use serde::Deserialize;

const REFERENCE_FIXTURE_JSON: &str = include_str!("../../../fixtures/reference-locators.json");

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
    pub locator: Option<NormalizedNostrEntity>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceFixtureOutcome {
    Public,
    SecretKey,
    Malformed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NormalizedNostrEntity {
    pub variant: String,
    pub pubkey: Option<String>,
    pub id: Option<String>,
    pub author: Option<String>,
    pub event_kind: Option<u16>,
    pub identifier: Option<String>,
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
        assert_eq!(corpus.schema, 2);

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

        let public_variants = corpus
            .cases
            .iter()
            .filter_map(|case| {
                case.locator
                    .as_ref()
                    .map(|locator| locator.variant.as_str())
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            public_variants,
            BTreeSet::from(["coordinate", "event", "event_id", "profile", "pubkey"])
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
    fn only_public_entities_carry_locator_expectations() {
        for case in reference_fixtures().cases {
            match case.outcome {
                ReferenceFixtureOutcome::Public => {
                    case.locator
                        .expect("public fixture must carry an exact locator");
                }
                ReferenceFixtureOutcome::SecretKey | ReferenceFixtureOutcome::Malformed => {
                    assert!(
                        case.locator.is_none(),
                        "{} unexpectedly has a locator",
                        case.name
                    );
                }
            }
        }
    }
}
