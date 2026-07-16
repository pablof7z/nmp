//! Engine-free NIP-19 reference targets and ordinary demand planning.
//!
//! A reference target is authored protocol data. Planning turns that data
//! into closed [`Demand`] values, but never observes them: no engine, query
//! handle, renderer, callback, cache, or lifecycle owner exists in this
//! module. The component which needs resolved data decides whether to use the
//! returned plan and owns every observation it opens.

use nostr::{EventId, PublicKey, RelayUrl};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    relay::{classify_relay_host, RelayHostClass},
    AccessContext, Binding, Demand, Filter, IndexedTagName, NostrEntity, SourceAuthority,
};

/// Maximum number of network-authored relay hints one reference helper may
/// promote into explicit pinned authority.
///
/// The hints are optional acquisition aids, so an authored reference cannot
/// be allowed to mint unbounded explicit relay fan-out. Omitted valid hints
/// remain observable through [`ReferenceDemandPlan::discarded_relay_hints`]
/// rather than being silently presented as the complete authored hint set.
pub const MAX_REFERENCE_RELAY_HINTS: usize = 8;

/// A normalized public reference target.
///
/// Relay, author, and kind hint fields remain acquisition hints unless the
/// NIP-19 entity defines them as target identity. In particular, an event's
/// `kind_hint` never constrains the canonical event-id selection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
    /// Normalize one already-decoded public NIP-19 entity. Secret-key and
    /// malformed entities cannot reach this function because
    /// [`crate::decode_nostr_entity`] rejects them first.
    #[must_use]
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

    /// Stable semantic identity for component-local state. Hints are
    /// deliberately absent because they do not change the target.
    #[must_use]
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

    /// Build one canonical observation plus zero or more optional acquisition
    /// helpers without opening any observation.
    ///
    /// Only the canonical demand should supply rendered state. Helper demands
    /// may use safe relay/author hints to feed events into NMP's one canonical
    /// store; the canonical observation remains the winner authority.
    pub fn demand_plan(&self) -> Result<ReferenceDemandPlan, ReferencePlanError> {
        self.validate_identity()?;

        let selection = self.selection();
        let canonical_source = match self {
            Self::Profile { .. } | Self::Address { .. } => SourceAuthority::AuthorOutboxes,
            Self::Event { .. } => SourceAuthority::Public,
        };
        let canonical = Demand::new(selection.clone(), canonical_source, AccessContext::Public)
            .expect("a validated reference target always forms a compatible canonical demand");

        let (relay_hints, discarded_relay_hints) = self.safe_relay_hints();
        let mut helpers = Vec::new();
        if !relay_hints.is_empty() {
            helpers.push(
                Demand::new(
                    selection.clone(),
                    SourceAuthority::Pinned(relay_hints),
                    AccessContext::Public,
                )
                .expect("a nonempty admitted relay-hint set forms a pinned demand"),
            );
        }

        if let Self::Event {
            id,
            author_hint: Some(author),
            ..
        } = self
        {
            // An invalid optional author hint cannot poison an otherwise valid
            // exact-id target. It is untrusted acquisition metadata, so it is
            // simply ineligible to mint an AuthorOutboxes helper.
            if PublicKey::from_hex(author).is_ok() {
                let mut hinted = selection;
                hinted.authors = Some(literal(author.clone()));
                hinted.ids = Some(literal(id.clone()));
                helpers.push(
                    Demand::new(
                        hinted,
                        SourceAuthority::AuthorOutboxes,
                        AccessContext::Public,
                    )
                    .expect("a validated author-hinted helper binds authors"),
                );
            }
        }

        Ok(ReferenceDemandPlan {
            target_key: self.key(),
            canonical,
            helpers,
            discarded_relay_hints,
        })
    }

    fn validate_identity(&self) -> Result<(), ReferencePlanError> {
        match self {
            Self::Profile { pubkey, .. } => PublicKey::from_hex(pubkey).map(|_| ()).map_err(|_| {
                ReferencePlanError::InvalidProfilePublicKey {
                    got: pubkey.clone(),
                }
            }),
            Self::Event { id, .. } => EventId::from_hex(id)
                .map(|_| ())
                .map_err(|_| ReferencePlanError::InvalidEventId { got: id.clone() }),
            Self::Address { author, .. } => PublicKey::from_hex(author).map(|_| ()).map_err(|_| {
                ReferencePlanError::InvalidAddressAuthor {
                    got: author.clone(),
                }
            }),
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

    fn safe_relay_hints(&self) -> (BTreeSet<RelayUrl>, u32) {
        let raw = match self {
            Self::Profile { relay_hints, .. }
            | Self::Event { relay_hints, .. }
            | Self::Address { relay_hints, .. } => relay_hints,
        };

        let mut candidates = BTreeSet::new();
        let mut discarded = 0u32;
        for raw_hint in raw {
            let Ok(relay) = RelayUrl::parse(raw_hint) else {
                discarded = discarded.saturating_add(1);
                continue;
            };
            if !admits_network_relay_hint(&relay) {
                discarded = discarded.saturating_add(1);
                continue;
            }
            candidates.insert(relay);
        }
        let overflow = candidates.len().saturating_sub(MAX_REFERENCE_RELAY_HINTS);
        discarded = discarded.saturating_add(u32::try_from(overflow).unwrap_or(u32::MAX));
        let admitted = candidates
            .into_iter()
            .take(MAX_REFERENCE_RELAY_HINTS)
            .collect();
        (admitted, discarded)
    }
}

fn literal(value: String) -> Binding {
    Binding::Literal(BTreeSet::from([value]))
}

/// A malformed manually-constructed target cannot produce acquisition.
/// Decoded [`NostrEntity`] values do not encounter these paths, but the
/// validation remains necessary at FFI and other value-construction seams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferencePlanError {
    InvalidProfilePublicKey { got: String },
    InvalidEventId { got: String },
    InvalidAddressAuthor { got: String },
}

impl std::fmt::Display for ReferencePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfilePublicKey { got } => {
                write!(f, "invalid profile public key: {got:?}")
            }
            Self::InvalidEventId { got } => write!(f, "invalid event id: {got:?}"),
            Self::InvalidAddressAuthor { got } => {
                write!(f, "invalid address author public key: {got:?}")
            }
        }
    }
}

impl std::error::Error for ReferencePlanError {}

/// Ordinary live-query demands available for one reference target.
///
/// This is a pure value, not an observation or a resource session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceDemandPlan {
    pub target_key: String,
    pub canonical: Demand,
    pub helpers: Vec<Demand>,
    /// Malformed, unsafe, or over-bound raw relay hints which could not be
    /// promoted into the pinned helper. Exact duplicates are deduplicated and
    /// do not increment this count.
    pub discarded_relay_hints: u32,
}

/// Apply the secure default to a network-authored relay hint without I/O.
/// Operator-configured relays do not use this predicate because their
/// provenance is explicit local configuration.
fn admits_network_relay_hint(relay: &RelayUrl) -> bool {
    classify_relay_host(relay) == RelayHostClass::Public
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_nostr_entity;

    const NPUB: &str = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
    const NOTE: &str = "note1m99r7nwc0wdrkzldrqan96gklg5usqspq7z9696j6unf0ljnpxjspqfw99";
    const NADDR: &str =
        "naddr1qqxnzd3exgersv33xymnsve3qgs8suecw4luyht9ekff89x4uacneapk8r5dyk0gmn6uwwurf6u9rusrqsqqqa282m3gxt";

    #[test]
    fn shared_public_nip19_fixtures_lower_to_exact_canonical_demands() {
        let profile = ReferenceTarget::from_entity(decode_nostr_entity(NPUB).unwrap())
            .demand_plan()
            .unwrap();
        assert_eq!(profile.canonical.source, SourceAuthority::AuthorOutboxes);
        assert_eq!(profile.canonical.selection.kinds, Some(BTreeSet::from([0])));
        assert!(profile.canonical.selection.authors.is_some());
        assert_eq!(profile.canonical.selection.limit, Some(1));

        let event = ReferenceTarget::from_entity(decode_nostr_entity(NOTE).unwrap())
            .demand_plan()
            .unwrap();
        assert_eq!(event.canonical.source, SourceAuthority::Public);
        assert!(event.canonical.selection.ids.is_some());
        assert!(event.canonical.selection.authors.is_none());
        assert!(event.canonical.selection.kinds.is_none());

        let address = ReferenceTarget::from_entity(decode_nostr_entity(NADDR).unwrap())
            .demand_plan()
            .unwrap();
        assert_eq!(address.canonical.source, SourceAuthority::AuthorOutboxes);
        assert_eq!(
            address.canonical.selection.kinds,
            Some(BTreeSet::from([30_023]))
        );
        assert!(address.canonical.selection.authors.is_some());
        assert!(address
            .canonical
            .selection
            .tags
            .contains_key(&IndexedTagName::new('d').unwrap()));
    }

    #[test]
    fn event_hints_never_change_canonical_target_or_actual_kind_selection() {
        let target = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: Some("2".repeat(64)),
            kind_hint: Some(30_023),
            relay_hints: Vec::new(),
        };
        let plan = target.demand_plan().unwrap();
        assert!(plan.canonical.selection.authors.is_none());
        assert!(plan.canonical.selection.kinds.is_none());
        assert_eq!(plan.helpers.len(), 1);
        assert_eq!(plan.helpers[0].source, SourceAuthority::AuthorOutboxes);
        assert!(plan.helpers[0].selection.ids.is_some());
        assert!(plan.helpers[0].selection.authors.is_some());
        assert!(plan.helpers[0].selection.kinds.is_none());
    }

    #[test]
    fn relay_hints_are_canonical_deduplicated_safe_and_explicitly_bounded() {
        let mut relay_hints = vec![
            "wss://RELAY.EXAMPLE.com".to_string(),
            "wss://relay.example.com/".to_string(),
            "not a relay".to_string(),
            "ws://127.0.0.1:7777".to_string(),
            "ws://10.0.0.2".to_string(),
            "wss://hiddenservice.onion".to_string(),
        ];
        relay_hints.extend(
            (0..=MAX_REFERENCE_RELAY_HINTS).map(|index| format!("wss://relay-{index}.example.com")),
        );
        let mut reversed_hints = relay_hints.clone();
        reversed_hints.reverse();
        let plan = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: None,
            kind_hint: None,
            relay_hints,
        }
        .demand_plan()
        .unwrap();

        assert_eq!(plan.helpers.len(), 1);
        let SourceAuthority::Pinned(relays) = &plan.helpers[0].source else {
            panic!("safe relay hints must form one pinned helper")
        };
        assert_eq!(relays.len(), MAX_REFERENCE_RELAY_HINTS);
        assert!(relays.iter().all(admits_network_relay_hint));
        assert!(plan.discarded_relay_hints >= 4);

        let reversed = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: None,
            kind_hint: None,
            relay_hints: reversed_hints,
        }
        .demand_plan()
        .unwrap();
        assert_eq!(reversed.helpers, plan.helpers);
        assert_eq!(reversed.discarded_relay_hints, plan.discarded_relay_hints);
    }

    #[test]
    fn malformed_identity_values_cannot_produce_acquisition_plans() {
        assert!(matches!(
            ReferenceTarget::Profile {
                pubkey: "not-a-pubkey".to_string(),
                relay_hints: Vec::new(),
            }
            .demand_plan(),
            Err(ReferencePlanError::InvalidProfilePublicKey { .. })
        ));
        assert!(matches!(
            ReferenceTarget::Event {
                id: "not-an-event".to_string(),
                author_hint: None,
                kind_hint: None,
                relay_hints: Vec::new(),
            }
            .demand_plan(),
            Err(ReferencePlanError::InvalidEventId { .. })
        ));
        assert!(matches!(
            ReferenceTarget::Address {
                kind: 30_023,
                author: "not-an-author".to_string(),
                identifier: "article".to_string(),
                relay_hints: Vec::new(),
            }
            .demand_plan(),
            Err(ReferencePlanError::InvalidAddressAuthor { .. })
        ));
    }

    #[test]
    fn invalid_optional_author_hint_is_ignored_without_losing_exact_event_plan() {
        let plan = ReferenceTarget::Event {
            id: "1".repeat(64),
            author_hint: Some("not-an-author".to_string()),
            kind_hint: None,
            relay_hints: Vec::new(),
        }
        .demand_plan()
        .unwrap();
        assert!(plan.helpers.is_empty());
        assert_eq!(plan.canonical.source, SourceAuthority::Public);
        assert!(plan.canonical.selection.ids.is_some());
    }
}
