use std::collections::{BTreeMap, BTreeSet};

use nmp::{
    admits_network_relay_hint, AccessContext, Binding, Demand, Filter, IndexedTagName, NostrEntity,
    SourceAuthority,
};
use nostr::RelayUrl;

/// A normalized semantic target. Relay, author, and kind hint fields remain
/// acquisition hints unless the NIP-19 entity defines them as target identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ReferenceTarget {
    Profile {
        pubkey: String,
        relay_hints: Vec<String>,
    },
    Event {
        id: String,
        author_hint: Option<String>,
        kind_hint: Option<u16>,
        relay_hints: Vec<String>,
    },
    Address {
        kind: u16,
        author: String,
        identifier: String,
        relay_hints: Vec<String>,
    },
}

impl ReferenceTarget {
    pub fn from_entity(entity: NostrEntity) -> Self {
        match entity {
            NostrEntity::Pubkey { pubkey } => Self::Profile {
                pubkey,
                relay_hints: Vec::new(),
            },
            NostrEntity::Profile { pubkey, relays } => Self::Profile {
                pubkey,
                relay_hints: relays,
            },
            NostrEntity::EventId { id } => Self::Event {
                id,
                author_hint: None,
                kind_hint: None,
                relay_hints: Vec::new(),
            },
            NostrEntity::Event {
                id,
                author,
                kind,
                relays,
            } => Self::Event {
                id,
                author_hint: author,
                kind_hint: kind,
                relay_hints: relays,
            },
            NostrEntity::Coordinate {
                kind,
                author,
                identifier,
                relays,
            } => Self::Address {
                kind,
                author,
                identifier,
                relay_hints: relays,
            },
        }
    }

    /// Stable semantic identity used to deduplicate acquisition without
    /// collapsing separate authored occurrences.
    pub fn key(&self) -> String {
        match self {
            Self::Profile { pubkey, .. } => format!("profile:{pubkey}"),
            Self::Event { id, .. } => format!("event:{id}"),
            Self::Address {
                kind,
                author,
                identifier,
                ..
            } => format!("address:{kind}:{author}:{identifier}"),
        }
    }

    /// Build one canonical observation plus zero or more acquisition helpers.
    ///
    /// Only the canonical observation supplies rendered state. Helper queries
    /// may contact relay or author hints and feed events into NMP's one store;
    /// the canonical query then observes the store-selected row. This preserves
    /// hint usefulness without trusting hints as target facts or duplicating
    /// replacement/winner logic in the content layer.
    pub fn demand_plan(&self) -> ReferenceDemandPlan {
        let selection = self.selection();
        let canonical_source = match self {
            Self::Profile { .. } | Self::Address { .. } => SourceAuthority::AuthorOutboxes,
            Self::Event { .. } => SourceAuthority::Public,
        };
        let canonical = Demand::new(selection.clone(), canonical_source, AccessContext::Public)
            .expect("reference target always produces a compatible canonical demand");

        let mut helpers = Vec::new();
        if let Some(relays) = self.valid_relay_hints() {
            helpers.push(
                Demand::new(
                    selection.clone(),
                    SourceAuthority::Pinned(relays),
                    AccessContext::Public,
                )
                .expect("validated nonempty relay hints form a pinned demand"),
            );
        }

        if let Self::Event {
            id,
            author_hint: Some(author),
            ..
        } = self
        {
            let mut hinted = selection;
            hinted.authors = Some(literal(author.clone()));
            hinted.ids = Some(literal(id.clone()));
            helpers.push(
                Demand::new(
                    hinted,
                    SourceAuthority::AuthorOutboxes,
                    AccessContext::Public,
                )
                .expect("author-hinted helper binds authors"),
            );
        }

        ReferenceDemandPlan {
            target_key: self.key(),
            canonical,
            helpers,
        }
    }

    fn selection(&self) -> Filter {
        match self {
            Self::Profile { pubkey, .. } => Filter {
                kinds: Some(BTreeSet::from([0])),
                authors: Some(literal(pubkey.clone())),
                limit: Some(1),
                ..Filter::default()
            },
            Self::Event { id, .. } => Filter {
                ids: Some(literal(id.clone())),
                limit: Some(1),
                ..Filter::default()
            },
            Self::Address {
                kind,
                author,
                identifier,
                ..
            } => Filter {
                kinds: Some(BTreeSet::from([*kind])),
                authors: Some(literal(author.clone())),
                tags: BTreeMap::from([(
                    IndexedTagName::new('d').expect("d is an indexed tag"),
                    literal(identifier.clone()),
                )]),
                limit: Some(1),
                ..Filter::default()
            },
        }
    }

    fn valid_relay_hints(&self) -> Option<BTreeSet<RelayUrl>> {
        let raw = match self {
            Self::Profile { relay_hints, .. }
            | Self::Event { relay_hints, .. }
            | Self::Address { relay_hints, .. } => relay_hints,
        };
        let relays: BTreeSet<RelayUrl> = raw
            .iter()
            .filter_map(|relay| RelayUrl::parse(relay).ok())
            .filter(admits_network_relay_hint)
            .collect();
        (!relays.is_empty()).then_some(relays)
    }
}

fn literal(value: String) -> Binding {
    Binding::Literal(BTreeSet::from([value]))
}

/// Ordinary live-query demands needed for one reference target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceDemandPlan {
    pub target_key: String,
    pub canonical: Demand,
    pub helpers: Vec<Demand>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_uses_outbox_canonical_and_pinned_hint_helper() {
        let target = ReferenceTarget::Profile {
            pubkey: "a".repeat(64),
            relay_hints: vec!["wss://relay.example.com".to_string()],
        };
        let plan = target.demand_plan();
        assert_eq!(plan.canonical.source, SourceAuthority::AuthorOutboxes);
        assert_eq!(plan.helpers.len(), 1);
        assert!(matches!(plan.helpers[0].source, SourceAuthority::Pinned(_)));
    }

    #[test]
    fn event_author_hint_is_helper_not_canonical_match_constraint() {
        let target = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: Some("2".repeat(64)),
            kind_hint: Some(1),
            relay_hints: Vec::new(),
        };
        let plan = target.demand_plan();
        assert_eq!(plan.canonical.source, SourceAuthority::Public);
        assert!(plan.canonical.selection.authors.is_none());
        assert_eq!(plan.canonical.selection.kinds, None);
        assert_eq!(plan.helpers.len(), 1);
        assert_eq!(plan.helpers[0].source, SourceAuthority::AuthorOutboxes);
    }

    #[test]
    fn malformed_relay_hints_are_ignored_without_losing_fallback() {
        let target = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: None,
            kind_hint: None,
            relay_hints: vec!["not a relay".to_string()],
        };
        let plan = target.demand_plan();
        assert!(plan.helpers.is_empty());
        assert_eq!(plan.canonical.source, SourceAuthority::Public);
    }

    #[test]
    fn network_authored_local_and_onion_hints_never_become_explicit_pinned_authority() {
        let target = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: None,
            kind_hint: None,
            relay_hints: vec![
                "ws://127.0.0.1:7777".to_string(),
                "ws://10.0.0.2".to_string(),
                "wss://hiddenservice.onion".to_string(),
            ],
        };
        let plan = target.demand_plan();
        assert!(plan.helpers.is_empty());
        assert_eq!(plan.canonical.source, SourceAuthority::Public);
    }
}
