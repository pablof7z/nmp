//! Pending-only admission and exact withdrawal falsifiers for #1342.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, IndexedTagName, RelaySessionKey,
    RoutingEvidence, RoutingEvidenceKind, SourceAuthority,
};
use nmp_router::{
    AdvertisedRelayLimits, CompileBudget, DemandKey, Router, RuleRegistry, Shortfall,
    ShortfallReason, WireOp,
};
use nmp_router_testkit::FixtureRoutingFacts;
use nostr::{Keys, PublicKey, RelayUrl};

fn atom(relay: &RelayUrl, value: &str) -> ContextualAtom {
    atom_on(BTreeSet::from([relay.clone()]), value)
}

fn atom_on(relays: BTreeSet<RelayUrl>, value: &str) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').unwrap(),
                BTreeSet::from([value.to_owned()]),
            )]),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(relays),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn routeless_outbox_atom(author: PublicKey) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn projected_outbox_atom(
    authors: BTreeSet<PublicKey>,
    relays: impl IntoIterator<Item = RelayUrl>,
) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(authors.iter().map(PublicKey::to_hex).collect()),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: relays
            .into_iter()
            .map(|relay| RoutingEvidence {
                relay,
                origin: RoutingEvidenceKind::Hint,
            })
            .collect(),
    }
}

fn incompatible_atom(relay: &RelayUrl, value: &str) -> ContextualAtom {
    let mut atom = atom(relay, value);
    atom.filter.limit = Some(1);
    atom
}

fn pinned_kind_atom(relay: &RelayUrl, kinds: impl IntoIterator<Item = u16>) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(kinds.into_iter().collect()),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn pinned_author_kind_atom(
    relay: &RelayUrl,
    kinds: impl IntoIterator<Item = u16>,
    authors: impl IntoIterator<Item = PublicKey>,
) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(kinds.into_iter().collect()),
            authors: Some(authors.into_iter().map(|author| author.to_hex()).collect()),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn subscription_budget(relay: &RelayUrl, max_subscriptions: usize) -> CompileBudget {
    CompileBudget::with_relay_cap(20).advertising(
        relay.clone(),
        AdvertisedRelayLimits {
            max_subscriptions: Some(max_subscriptions),
            max_subid_length: None,
        },
    )
}

fn reqs(delta: &nmp_router::AdmissionOutcome) -> usize {
    delta
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .filter(|op| matches!(op, WireOp::Req(_, _)))
        .count()
}

fn withdraw(
    router: &mut Router,
    atoms: impl IntoIterator<Item = ContextualAtom>,
    cap: usize,
) -> nmp_router::WireDelta {
    router.withdraw(atoms, cap).wire
}

#[path = "admission/cap_assignment.rs"]
mod cap_assignment;
#[path = "admission/coverage_behavior.rs"]
mod coverage_behavior;
#[path = "admission/lifecycle.rs"]
mod lifecycle;
#[path = "admission/scale_withdrawal.rs"]
mod scale_withdrawal;
#[path = "admission/shortfall_recovery.rs"]
mod shortfall_recovery;
