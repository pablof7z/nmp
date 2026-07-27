//! #879: one shared NIP-19/NIP-21 corpus through the direct Rust and
//! UniFFI codec/content-parser surfaces.
//!
//! No engine is constructed and no demand can be produced. Authored locator
//! fields remain exact data: a bare `npub` stays distinct from `nprofile`,
//! and relay/author/kind hints acquire no schema or network authority here.

use nmp::{NostrEntity, NostrEntityError};
use nmp_content::{parse_content, ContentSyntax, InlineNode};
use nmp_ffi::content::{parse_nostr_content, FfiContentSyntax, FfiInlineNode};
use nmp_ffi::convert::FfiError;
use nmp_ffi::types::FfiNostrEntity;
use nmp_test_support::reference_fixtures::{
    reference_fixtures, NormalizedNostrEntity, ReferenceFixtureOutcome,
};

#[test]
fn shared_nip19_fixtures_preserve_exact_locators_across_rust_and_ffi() {
    for fixture in reference_fixtures().cases {
        match fixture.outcome {
            ReferenceFixtureOutcome::Public => {
                let expected = fixture
                    .locator
                    .as_ref()
                    .expect("public fixture must carry an exact locator");

                let direct = nmp::decode_nostr_entity(&fixture.input)
                    .expect("public direct locator must decode");
                let ffi = nmp_ffi::entity::decode_nostr_entity(fixture.input.clone())
                    .expect("public FFI locator must decode");
                assert_eq!(
                    normalize_entity(direct),
                    *expected,
                    "{} direct locator drifted",
                    fixture.name
                );
                assert_eq!(
                    normalize_ffi_entity(ffi),
                    *expected,
                    "{} FFI locator drifted",
                    fixture.name
                );

                if let Some(bare) = fixture.input.strip_prefix("nostr:") {
                    assert_eq!(
                        normalize_entity(nmp::decode_nostr_entity(bare).unwrap()),
                        *expected,
                        "{} direct URI and bare forms diverged",
                        fixture.name
                    );
                    assert_eq!(
                        normalize_ffi_entity(
                            nmp_ffi::entity::decode_nostr_entity(bare.to_string()).unwrap(),
                        ),
                        *expected,
                        "{} FFI URI and bare forms diverged",
                        fixture.name
                    );
                }

                let direct_document = parse_content(&fixture.input, ContentSyntax::PlainText);
                let direct_occurrences = direct_document.references();
                assert_eq!(
                    direct_occurrences.len(),
                    1,
                    "{} must parse as one direct-Rust occurrence",
                    fixture.name
                );
                assert_eq!(
                    normalize_entity(direct_occurrences[0].target.clone()),
                    *expected,
                    "{} direct content locator drifted",
                    fixture.name
                );

                let ffi_document =
                    parse_nostr_content(fixture.input.clone(), FfiContentSyntax::PlainText);
                let ffi_locators = ffi_document
                    .blocks
                    .into_iter()
                    .flat_map(|block| block.inlines)
                    .filter_map(|inline| match inline {
                        FfiInlineNode::Reference { occurrence, .. } => Some(occurrence.target),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    ffi_locators.len(),
                    1,
                    "{} must parse as one FFI occurrence",
                    fixture.name
                );
                assert_eq!(
                    normalize_ffi_entity(ffi_locators.into_iter().next().unwrap()),
                    *expected,
                    "{} FFI content locator drifted",
                    fixture.name
                );
            }
            ReferenceFixtureOutcome::SecretKey | ReferenceFixtureOutcome::Malformed => {
                assert_non_actionable(&fixture.name, &fixture.input, fixture.outcome);
            }
        }
    }
}

#[test]
fn bare_pubkey_and_authored_profile_remain_different_variants() {
    let corpus = reference_fixtures();
    let npub = corpus
        .cases
        .iter()
        .find(|fixture| fixture.name == "npub-public-key")
        .and_then(|fixture| fixture.locator.as_ref())
        .unwrap();
    let nprofile = corpus
        .cases
        .iter()
        .find(|fixture| fixture.name == "nprofile-relay-hints")
        .and_then(|fixture| fixture.locator.as_ref())
        .unwrap();

    assert_eq!(npub.variant, "pubkey");
    assert_eq!(nprofile.variant, "profile");
}

fn assert_non_actionable(name: &str, input: &str, outcome: ReferenceFixtureOutcome) {
    let direct_document = parse_content(input, ContentSyntax::PlainText);
    assert!(
        direct_document.references().is_empty(),
        "{name} unexpectedly produced a direct locator"
    );
    assert_eq!(direct_visible_text(&direct_document.blocks), input);

    let ffi_document = parse_nostr_content(input.to_string(), FfiContentSyntax::PlainText);
    assert!(
        ffi_document.blocks.iter().all(|block| block
            .inlines
            .iter()
            .all(|inline| !matches!(inline, FfiInlineNode::Reference { .. }))),
        "{name} unexpectedly produced an FFI locator"
    );
    assert_eq!(ffi_visible_text(&ffi_document.blocks), input);

    match outcome {
        ReferenceFixtureOutcome::SecretKey => {
            assert_eq!(
                nmp::decode_nostr_entity(input),
                Err(NostrEntityError::SecretKeyRejected)
            );
            assert_eq!(
                nmp_ffi::entity::decode_nostr_entity(input.to_string()),
                Err(FfiError::NostrEntitySecretKeyRejected)
            );
        }
        ReferenceFixtureOutcome::Malformed => {
            assert!(matches!(
                nmp::decode_nostr_entity(input),
                Err(NostrEntityError::Malformed { .. })
            ));
            assert!(matches!(
                nmp_ffi::entity::decode_nostr_entity(input.to_string()),
                Err(FfiError::InvalidNostrEntity { .. })
            ));
        }
        ReferenceFixtureOutcome::Public => unreachable!(),
    }
}

fn direct_visible_text(blocks: &[nmp_content::ContentBlock]) -> String {
    blocks
        .iter()
        .flat_map(|block| &block.inlines)
        .filter_map(|inline| match inline {
            InlineNode::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn ffi_visible_text(blocks: &[nmp_ffi::content::FfiContentBlock]) -> String {
    blocks
        .iter()
        .flat_map(|block| &block.inlines)
        .filter_map(|inline| match inline {
            FfiInlineNode::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn normalize_entity(entity: NostrEntity) -> NormalizedNostrEntity {
    match entity {
        NostrEntity::Pubkey { pubkey } => {
            normalized("pubkey", Some(pubkey), None, None, None, None, vec![])
        }
        NostrEntity::Profile { pubkey, relays } => {
            normalized("profile", Some(pubkey), None, None, None, None, relays)
        }
        NostrEntity::EventId { id } => {
            normalized("event_id", None, Some(id), None, None, None, vec![])
        }
        NostrEntity::Event {
            id,
            author,
            kind,
            relays,
        } => normalized("event", None, Some(id), author, kind, None, relays),
        NostrEntity::Coordinate {
            kind,
            author,
            identifier,
            relays,
        } => normalized(
            "coordinate",
            None,
            None,
            Some(author),
            Some(kind),
            Some(identifier),
            relays,
        ),
    }
}

fn normalize_ffi_entity(entity: FfiNostrEntity) -> NormalizedNostrEntity {
    match entity {
        FfiNostrEntity::Pubkey { pubkey } => {
            normalized("pubkey", Some(pubkey), None, None, None, None, vec![])
        }
        FfiNostrEntity::Profile { pubkey, relays } => {
            normalized("profile", Some(pubkey), None, None, None, None, relays)
        }
        FfiNostrEntity::EventId { id } => {
            normalized("event_id", None, Some(id), None, None, None, vec![])
        }
        FfiNostrEntity::Event {
            id,
            author,
            kind,
            relays,
        } => normalized("event", None, Some(id), author, kind, None, relays),
        FfiNostrEntity::Coordinate {
            kind,
            author,
            identifier,
            relays,
        } => normalized(
            "coordinate",
            None,
            None,
            Some(author),
            Some(kind),
            Some(identifier),
            relays,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn normalized(
    variant: &str,
    pubkey: Option<String>,
    id: Option<String>,
    author: Option<String>,
    event_kind: Option<u16>,
    identifier: Option<String>,
    relays: Vec<String>,
) -> NormalizedNostrEntity {
    NormalizedNostrEntity {
        variant: variant.to_string(),
        pubkey,
        id,
        author,
        event_kind,
        identifier,
        relays,
    }
}
