//! #583: one shared NIP-19 corpus through the direct Rust and UniFFI
//! parser/reference surfaces. No engine is constructed: parsing and demand
//! planning are pure values and cannot open observations or relay work.

use std::collections::BTreeMap;

use nmp::{AccessContext, Binding, CacheMode, Demand, NostrEntityError, SourceAuthority};
use nmp_bdd::reference_fixtures::{
    reference_fixtures, NormalizedDemand, NormalizedFilter, NormalizedReferencePlan,
    NormalizedReferenceTarget, NormalizedSource, ReferenceFixtureOutcome,
};
use nmp_content::{parse_content, ContentSyntax, InlineNode};
use nmp_ffi::content::{parse_nostr_content, FfiContentSyntax, FfiInlineNode};
use nmp_ffi::convert::FfiError;
use nmp_ffi::reference::{
    reference_demand_plan as ffi_reference_demand_plan, FfiReferenceDemandPlan, FfiReferenceTarget,
};
use nmp_ffi::types::FfiNostrEntity;
use nmp_ffi::types::{FfiAccessContext, FfiBinding, FfiCacheMode, FfiDemand, FfiSourceAuthority};
use nmp_grammar::reference::{ReferenceDemandPlan, ReferenceTarget};

#[test]
fn shared_nip19_fixtures_match_direct_rust_and_ffi_exactly() {
    for fixture in reference_fixtures().cases {
        match fixture.outcome {
            ReferenceFixtureOutcome::Public => {
                let expected_target = fixture
                    .target
                    .as_ref()
                    .expect("public fixture must carry a target");
                let expected_plan = fixture
                    .plan
                    .as_ref()
                    .expect("public fixture must carry a plan");

                let direct_entity = nmp::decode_nostr_entity(&fixture.input)
                    .expect("public direct entity must decode");
                let ffi_entity = nmp_ffi::entity::decode_nostr_entity(fixture.input.clone())
                    .expect("public FFI entity must decode");
                assert_eq!(
                    normalize_entity(direct_entity),
                    *expected_target,
                    "{} direct entity projection drifted",
                    fixture.name
                );
                assert_eq!(
                    normalize_ffi_entity(ffi_entity),
                    *expected_target,
                    "{} FFI entity projection drifted",
                    fixture.name
                );
                if let Some(bare) = fixture.input.strip_prefix("nostr:") {
                    assert_eq!(
                        normalize_entity(nmp::decode_nostr_entity(bare).unwrap()),
                        *expected_target,
                        "{} direct nostr URI and bare forms diverged",
                        fixture.name
                    );
                    assert_eq!(
                        normalize_ffi_entity(
                            nmp_ffi::entity::decode_nostr_entity(bare.to_string()).unwrap(),
                        ),
                        *expected_target,
                        "{} FFI nostr URI and bare forms diverged",
                        fixture.name
                    );
                }

                let direct_document = parse_content(&fixture.input, ContentSyntax::PlainText);
                let direct_occurrences = direct_document.references();
                assert_eq!(
                    direct_occurrences.len(),
                    1,
                    "{} must parse as one direct-Rust reference",
                    fixture.name
                );
                let direct_target = &direct_occurrences[0].target;
                let direct_normalized_target = normalize_target(direct_target);
                let direct_normalized_plan = normalize_plan(
                    direct_target
                        .demand_plan()
                        .expect("decoded public target must plan"),
                );

                let ffi_document =
                    parse_nostr_content(fixture.input.clone(), FfiContentSyntax::PlainText);
                let ffi_targets = ffi_document
                    .blocks
                    .into_iter()
                    .flat_map(|block| block.inlines)
                    .filter_map(|inline| match inline {
                        FfiInlineNode::Reference { occurrence, .. } => Some(occurrence.target),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    ffi_targets.len(),
                    1,
                    "{} must parse as one FFI reference",
                    fixture.name
                );
                let ffi_target = ffi_targets.into_iter().next().unwrap();
                let ffi_normalized_target = normalize_ffi_target(&ffi_target);
                let ffi_normalized_plan = normalize_ffi_plan(
                    ffi_reference_demand_plan(ffi_target)
                        .expect("decoded public FFI target must plan"),
                );

                assert_eq!(
                    &direct_normalized_target, expected_target,
                    "{} direct target drifted from the shared oracle",
                    fixture.name
                );
                assert_eq!(
                    &ffi_normalized_target, expected_target,
                    "{} FFI target drifted from the shared oracle",
                    fixture.name
                );
                assert_eq!(direct_normalized_target, ffi_normalized_target);
                assert_eq!(
                    &direct_normalized_plan, expected_plan,
                    "{} direct demand plan drifted from the shared oracle",
                    fixture.name
                );
                assert_eq!(
                    &ffi_normalized_plan, expected_plan,
                    "{} FFI demand plan drifted from the shared oracle",
                    fixture.name
                );
                assert_eq!(direct_normalized_plan, ffi_normalized_plan);
            }
            ReferenceFixtureOutcome::SecretKey | ReferenceFixtureOutcome::Malformed => {
                assert_non_actionable(&fixture.name, &fixture.input, fixture.outcome);
            }
        }
    }
}

fn assert_non_actionable(name: &str, input: &str, outcome: ReferenceFixtureOutcome) {
    let direct_document = parse_content(input, ContentSyntax::PlainText);
    assert!(
        direct_document.references().is_empty(),
        "{name} unexpectedly produced a direct target"
    );
    assert_eq!(direct_visible_text(&direct_document.blocks), input);

    let ffi_document = parse_nostr_content(input.to_string(), FfiContentSyntax::PlainText);
    assert!(
        ffi_document.blocks.iter().all(|block| block
            .inlines
            .iter()
            .all(|inline| !matches!(inline, FfiInlineNode::Reference { .. }))),
        "{name} unexpectedly produced an FFI target"
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

fn normalize_target(target: &ReferenceTarget) -> NormalizedReferenceTarget {
    match target {
        ReferenceTarget::Profile {
            pubkey,
            relay_hints,
        } => NormalizedReferenceTarget {
            kind: "profile".to_string(),
            key: target.key(),
            pubkey: Some(pubkey.clone()),
            id: None,
            author_hint: None,
            kind_hint: None,
            address_kind: None,
            author: None,
            identifier: None,
            relay_hints: relay_hints.clone(),
        },
        ReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        } => NormalizedReferenceTarget {
            kind: "event".to_string(),
            key: target.key(),
            pubkey: None,
            id: Some(id.clone()),
            author_hint: author_hint.clone(),
            kind_hint: *kind_hint,
            address_kind: None,
            author: None,
            identifier: None,
            relay_hints: relay_hints.clone(),
        },
        ReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        } => NormalizedReferenceTarget {
            kind: "address".to_string(),
            key: target.key(),
            pubkey: None,
            id: None,
            author_hint: None,
            kind_hint: None,
            address_kind: Some(*kind),
            author: Some(author.clone()),
            identifier: Some(identifier.clone()),
            relay_hints: relay_hints.clone(),
        },
    }
}

fn normalize_entity(entity: nmp::NostrEntity) -> NormalizedReferenceTarget {
    normalize_target(&ReferenceTarget::from_entity(entity))
}

fn normalize_ffi_entity(entity: FfiNostrEntity) -> NormalizedReferenceTarget {
    match entity {
        FfiNostrEntity::Pubkey { pubkey } => normalize_ffi_target(&FfiReferenceTarget::Profile {
            pubkey,
            relay_hints: Vec::new(),
        }),
        FfiNostrEntity::Profile { pubkey, relays } => {
            normalize_ffi_target(&FfiReferenceTarget::Profile {
                pubkey,
                relay_hints: relays,
            })
        }
        FfiNostrEntity::EventId { id } => normalize_ffi_target(&FfiReferenceTarget::Event {
            id,
            author_hint: None,
            kind_hint: None,
            relay_hints: Vec::new(),
        }),
        FfiNostrEntity::Event {
            id,
            author,
            kind,
            relays,
        } => normalize_ffi_target(&FfiReferenceTarget::Event {
            id,
            author_hint: author,
            kind_hint: kind,
            relay_hints: relays,
        }),
        FfiNostrEntity::Coordinate {
            kind,
            author,
            identifier,
            relays,
        } => normalize_ffi_target(&FfiReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints: relays,
        }),
    }
}

fn normalize_ffi_target(target: &FfiReferenceTarget) -> NormalizedReferenceTarget {
    match target {
        FfiReferenceTarget::Profile {
            pubkey,
            relay_hints,
        } => NormalizedReferenceTarget {
            kind: "profile".to_string(),
            key: format!("profile:{pubkey}"),
            pubkey: Some(pubkey.clone()),
            id: None,
            author_hint: None,
            kind_hint: None,
            address_kind: None,
            author: None,
            identifier: None,
            relay_hints: relay_hints.clone(),
        },
        FfiReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        } => NormalizedReferenceTarget {
            kind: "event".to_string(),
            key: format!("event:{id}"),
            pubkey: None,
            id: Some(id.clone()),
            author_hint: author_hint.clone(),
            kind_hint: *kind_hint,
            address_kind: None,
            author: None,
            identifier: None,
            relay_hints: relay_hints.clone(),
        },
        FfiReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        } => NormalizedReferenceTarget {
            kind: "address".to_string(),
            key: format!("address:{kind}:{author}:{identifier}"),
            pubkey: None,
            id: None,
            author_hint: None,
            kind_hint: None,
            address_kind: Some(*kind),
            author: Some(author.clone()),
            identifier: Some(identifier.clone()),
            relay_hints: relay_hints.clone(),
        },
    }
}

fn normalize_plan(plan: ReferenceDemandPlan) -> NormalizedReferencePlan {
    NormalizedReferencePlan {
        target_key: plan.target_key,
        canonical: normalize_demand(plan.canonical),
        helpers: plan.helpers.into_iter().map(normalize_demand).collect(),
        discarded_relay_hints: plan.discarded_relay_hints,
    }
}

fn normalize_ffi_plan(plan: FfiReferenceDemandPlan) -> NormalizedReferencePlan {
    NormalizedReferencePlan {
        target_key: plan.target_key,
        canonical: normalize_ffi_demand(plan.canonical),
        helpers: plan.helpers.into_iter().map(normalize_ffi_demand).collect(),
        discarded_relay_hints: plan.discarded_relay_hints,
    }
}

fn normalize_demand(demand: Demand) -> NormalizedDemand {
    let source = match demand.source {
        SourceAuthority::AuthorOutboxes => NormalizedSource {
            kind: "author_outboxes".to_string(),
            relays: Vec::new(),
        },
        SourceAuthority::Public => NormalizedSource {
            kind: "public".to_string(),
            relays: Vec::new(),
        },
        SourceAuthority::Pinned(relays) => NormalizedSource {
            kind: "pinned".to_string(),
            relays: relays.into_iter().map(|relay| relay.to_string()).collect(),
        },
    };
    let access = match demand.access {
        AccessContext::Public => "public".to_string(),
        AccessContext::Nip42(public_key) => format!("nip42:{}", public_key.to_hex()),
    };
    let cache = match demand.cache {
        CacheMode::Agnostic => "agnostic".to_string(),
        CacheMode::Strict => "strict".to_string(),
    };
    NormalizedDemand {
        selection: NormalizedFilter {
            kinds: demand
                .selection
                .kinds
                .unwrap_or_default()
                .into_iter()
                .collect(),
            authors: direct_literal(demand.selection.authors),
            ids: direct_literal(demand.selection.ids),
            tags: demand
                .selection
                .tags
                .into_iter()
                .map(|(name, binding)| (name.as_char().to_string(), direct_literal(Some(binding))))
                .collect(),
            since: demand.selection.since,
            until: demand.selection.until,
            limit: demand.selection.limit.map(|limit| {
                u32::try_from(limit).expect("reference limit must fit the public FFI width")
            }),
        },
        source,
        access,
        cache,
    }
}

fn direct_literal(binding: Option<Binding>) -> Vec<String> {
    match binding {
        None => Vec::new(),
        Some(Binding::Literal(values)) => values.into_iter().collect(),
        Some(other) => panic!("reference plan unexpectedly emitted non-literal binding: {other:?}"),
    }
}

fn normalize_ffi_demand(demand: FfiDemand) -> NormalizedDemand {
    let source = match demand.source {
        FfiSourceAuthority::AuthorOutboxes => NormalizedSource {
            kind: "author_outboxes".to_string(),
            relays: Vec::new(),
        },
        FfiSourceAuthority::Public => NormalizedSource {
            kind: "public".to_string(),
            relays: Vec::new(),
        },
        FfiSourceAuthority::Pinned { mut relays } => {
            relays.sort();
            NormalizedSource {
                kind: "pinned".to_string(),
                relays,
            }
        }
    };
    let access = match demand.access {
        FfiAccessContext::Public => "public".to_string(),
        FfiAccessContext::Nip42 { public_key } => format!("nip42:{public_key}"),
    };
    let cache = match demand.cache {
        FfiCacheMode::Agnostic => "agnostic".to_string(),
        FfiCacheMode::Strict => "strict".to_string(),
    };
    NormalizedDemand {
        selection: NormalizedFilter {
            kinds: demand.selection.kinds.unwrap_or_default(),
            authors: ffi_literal(demand.selection.authors),
            ids: ffi_literal(demand.selection.ids),
            tags: demand
                .selection
                .tags
                .into_iter()
                .map(|(name, binding)| (name, ffi_literal(Some(binding))))
                .collect::<BTreeMap<_, _>>(),
            since: demand.selection.since,
            until: demand.selection.until,
            limit: demand.selection.limit,
        },
        source,
        access,
        cache,
    }
}

fn ffi_literal(binding: Option<FfiBinding>) -> Vec<String> {
    match binding {
        None => Vec::new(),
        Some(FfiBinding::Literal { mut values }) => {
            values.sort();
            values
        }
        Some(other) => {
            panic!("reference FFI plan unexpectedly emitted non-literal binding: {other:?}")
        }
    }
}
