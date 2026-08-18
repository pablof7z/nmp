//! Protocol-owned NIP-65 account bootstrap.
//!
//! A brand-new author has no neutral route fact yet, so an ordinary automatic
//! publication parks while its destinations are unknown.
//! [`BootstrapRelayList::into_write_intent`] composes the author's first
//! relay list to a validated exact relay set instead — an
//! [`WriteRouting::Explicit`] minted by this crate, the same "protocol
//! crate mints an exact route" pattern any other crate uses, running through
//! the ordinary durable acceptance, signer, outbox, and tracked receipt
//! pipeline. No dedicated routing variant is involved, and none is needed.
//!
//! The operation never mutates NMP's neutral fact store and never inserts a
//! synthetic network row or provenance fact. The new kind:10002 becomes a
//! routing fact only after it returns through the optional facade's ordinary
//! query and this crate selects it as the canonical replaceable winner. Every
//! later write uses ordinary automatic routing.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    Binding, Demand, EventBuilder, Filter, Identity, ReadRouting, WriteIntent,
    WritePayload, WriteRouting,
};
use nostr::nips::nip65::RelayMetadata;
use nostr::{Event, EventId, Kind, PublicKey, RelayUrl, Tag};

/// Maximum number of exact relays the bootstrap publication may contact.
///
/// This is a protocol-operation bound, not a promise that an engine configured
/// with a smaller physical relay ceiling can connect to all of them
/// simultaneously. Such transport state remains ordinary receipt evidence.
pub const MAX_BOOTSTRAP_RELAYS: usize = 8;

/// Maximum number of relay rows carried by the first kind:10002.
pub const MAX_RELAY_LIST_ENTRIES: usize = 32;

/// NIP-65's meaning for one `r` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayUsage {
    /// An unmarked row: the relay is both readable and writable.
    ReadWrite,
    /// An explicitly read-only row.
    Read,
    /// An explicitly write-only row.
    Write,
}

impl RelayUsage {
    fn is_write_capable(self) -> bool {
        matches!(self, Self::ReadWrite | Self::Write)
    }

    fn metadata(self) -> Option<RelayMetadata> {
        match self {
            Self::ReadWrite => None,
            Self::Read => Some(RelayMetadata::Read),
            Self::Write => Some(RelayMetadata::Write),
        }
    }
}

/// One validated-URL relay row advertised by the new account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayListEntry {
    relay: RelayUrl,
    usage: RelayUsage,
}

impl RelayListEntry {
    pub fn new(relay: RelayUrl, usage: RelayUsage) -> Self {
        Self { relay, usage }
    }

    pub fn relay(&self) -> &RelayUrl {
        &self.relay
    }

    pub fn usage(&self) -> RelayUsage {
        self.usage
    }
}

/// Fully validated semantic input for the first kind:10002 publication.
///
/// `bootstrap_relays` are the exact delivery targets for this one write.
/// `relay_list` is the independent NIP-65 policy the event advertises. Keeping
/// them separate lets an account seed discovery through a known bootstrap
/// relay without falsely declaring that relay to be one of its long-term
/// outboxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapRelayList {
    author: PublicKey,
    bootstrap_relays: BTreeSet<RelayUrl>,
    relay_list: Vec<RelayListEntry>,
}

impl BootstrapRelayList {
    /// Validate the complete operation before any write can be accepted.
    pub fn new(
        author: PublicKey,
        bootstrap_relays: Vec<RelayUrl>,
        relay_list: Vec<RelayListEntry>,
    ) -> Result<Self, BootstrapRelayListError> {
        if bootstrap_relays.is_empty() {
            return Err(BootstrapRelayListError::EmptyBootstrapRelays);
        }
        if bootstrap_relays.len() > MAX_BOOTSTRAP_RELAYS {
            return Err(BootstrapRelayListError::TooManyBootstrapRelays {
                actual: bootstrap_relays.len(),
                max: MAX_BOOTSTRAP_RELAYS,
            });
        }
        let mut exact_bootstrap_relays = BTreeSet::new();
        for relay in bootstrap_relays {
            if !exact_bootstrap_relays.insert(relay.clone()) {
                return Err(BootstrapRelayListError::DuplicateBootstrapRelay { relay });
            }
        }

        if relay_list.is_empty() {
            return Err(BootstrapRelayListError::EmptyRelayList);
        }
        if relay_list.len() > MAX_RELAY_LIST_ENTRIES {
            return Err(BootstrapRelayListError::TooManyRelayListEntries {
                actual: relay_list.len(),
                max: MAX_RELAY_LIST_ENTRIES,
            });
        }
        let mut advertised = BTreeSet::new();
        let mut has_write_capable = false;
        for entry in &relay_list {
            if !advertised.insert(entry.relay.clone()) {
                return Err(BootstrapRelayListError::DuplicateRelayListRelay {
                    relay: entry.relay.clone(),
                });
            }
            has_write_capable |= entry.usage.is_write_capable();
        }
        if !has_write_capable {
            return Err(BootstrapRelayListError::NoWriteCapableRelay);
        }

        Ok(Self {
            author,
            bootstrap_relays: exact_bootstrap_relays,
            relay_list,
        })
    }

    pub fn author(&self) -> PublicKey {
        self.author
    }

    pub fn bootstrap_relays(&self) -> impl ExactSizeIterator<Item = &RelayUrl> {
        self.bootstrap_relays.iter()
    }

    pub fn relay_list(&self) -> &[RelayListEntry] {
        &self.relay_list
    }

    /// Mint the ordinary exact write intent. Engine binding belongs to the
    /// optional facade assembly, not this protocol crate.
    pub fn into_write_intent(self) -> WriteIntent {
        compose_relay_list_bootstrap(self)
    }
}

/// Refusals while validating the pure operation value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapRelayListError {
    EmptyBootstrapRelays,
    TooManyBootstrapRelays { actual: usize, max: usize },
    DuplicateBootstrapRelay { relay: RelayUrl },
    EmptyRelayList,
    TooManyRelayListEntries { actual: usize, max: usize },
    DuplicateRelayListRelay { relay: RelayUrl },
    NoWriteCapableRelay,
}

impl std::fmt::Display for BootstrapRelayListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyBootstrapRelays => {
                f.write_str("NIP-65 bootstrap requires at least one delivery relay")
            }
            Self::TooManyBootstrapRelays { actual, max } => {
                write!(
                    f,
                    "NIP-65 bootstrap has {actual} delivery relays; maximum is {max}"
                )
            }
            Self::DuplicateBootstrapRelay { relay } => {
                write!(f, "NIP-65 bootstrap relay appears more than once: {relay}")
            }
            Self::EmptyRelayList => {
                f.write_str("the first NIP-65 relay list must contain at least one relay")
            }
            Self::TooManyRelayListEntries { actual, max } => {
                write!(
                    f,
                    "NIP-65 relay list has {actual} entries; maximum is {max}"
                )
            }
            Self::DuplicateRelayListRelay { relay } => {
                write!(f, "NIP-65 relay-list URL appears more than once: {relay}")
            }
            Self::NoWriteCapableRelay => {
                f.write_str("the first NIP-65 relay list must name a write-capable relay")
            }
        }
    }
}

impl std::error::Error for BootstrapRelayListError {}

fn compose_relay_list_bootstrap(request: BootstrapRelayList) -> WriteIntent {
    let BootstrapRelayList {
        author,
        bootstrap_relays,
        relay_list,
    } = request;
    let tags: Vec<Tag> = relay_list
        .into_iter()
        .map(|entry| Tag::relay_metadata(entry.relay, entry.usage.metadata()))
        .collect();
    WriteIntent {
        payload: WritePayload::Event(EventBuilder {
            kind: Kind::RelayList,
            tags,
            content: String::new(),
            created_at: None,
        }),
        routing: WriteRouting::Explicit(bootstrap_relays.into_iter().collect()),
        // The request names the account this bootstrap is FOR, and a
        // builder has no author of its own, so that name is now what
        // selects the signing identity rather than something the engine
        // has to compare a stamped author against.
        identity: Identity::Explicit(author),
    }
}

/// NIP-65 Relay List Metadata.
pub const RELAY_LIST_KIND: u16 = 10_002;

/// Build the coordinator's ordinary exact-source query.
///
/// No sources means no question was asked, so this returns `None` rather
/// than creating a source-less query that could later be mistaken for
/// absence.
pub fn relay_list_demand(
    authors: &BTreeSet<PublicKey>,
    sources: &BTreeSet<RelayUrl>,
) -> Option<Demand> {
    if authors.is_empty() || sources.is_empty() {
        return None;
    }
    Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([RELAY_LIST_KIND])),
            authors: Some(Binding::Literal(
                authors.iter().map(PublicKey::to_hex).collect(),
            )),
            ..Filter::default()
        },
        ReadRouting::Explicit(sources.iter().cloned().collect())
    )
    .ok()
}

/// The directional meaning of one admitted current relay-list winner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedAuthorRoutes {
    pub outbound: BTreeSet<RelayUrl>,
    pub inbound: BTreeSet<RelayUrl>,
}

/// Parse one winner after winner selection and before neutral fact mutation.
///
/// Unmarked rows enter both sets; `read` and `write` markers enter exactly
/// one. Unknown markers and malformed URLs enter neither set. Sets deduplicate
/// repeated URLs.
///
/// Nothing is admitted or refused here, deliberately (#1251). Admission
/// depends on WHOSE declaration this event is, and a parser holds one event
/// and no identity: it cannot tell the author's own relay list from a
/// stranger's, so any filtering it did would have to guess. Every refused row
/// used to vanish silently at exactly this point, which is how an author whose
/// list was entirely local became indistinguishable from an author who
/// declared no relays at all. The caller knows the identity and applies
/// admission with it.
pub fn parse_relay_list(event: &Event) -> ParsedAuthorRoutes {
    let mut routes = ParsedAuthorRoutes::default();
    for tag in event.tags.iter() {
        let values = tag.as_slice();
        if values.first().map(String::as_str) != Some("r") {
            continue;
        }
        let Some(value) = values.get(1) else {
            continue;
        };
        let Ok(relay) = RelayUrl::parse(value) else {
            continue;
        };
        match values.get(2).map(String::as_str) {
            None => {
                routes.outbound.insert(relay.clone());
                routes.inbound.insert(relay);
            }
            Some("read") => {
                routes.inbound.insert(relay);
            }
            Some("write") => {
                routes.outbound.insert(relay);
            }
            Some(_) => {}
        }
    }
    routes
}

/// One private-neutral replacement the facade must apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorUpdate {
    Present {
        author: PublicKey,
        routes: ParsedAuthorRoutes,
    },
    Absent {
        author: PublicKey,
    },
}

/// A rerooted ordinary query and the revision settlements must cite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorQuery {
    pub revision: u64,
    pub demand: Demand,
}

/// Pure owner of NIP-65 demand, winner, marker, and absence semantics.
pub struct Nip65Coordinator {
    sources: BTreeSet<RelayUrl>,
    authors: BTreeSet<PublicKey>,
    revision: u64,
    winners: BTreeMap<PublicKey, Event>,
    settled_sources: BTreeSet<RelayUrl>,
    absent_emitted: BTreeSet<PublicKey>,
}

impl Nip65Coordinator {
    pub fn new(sources: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            authors: BTreeSet::new(),
            revision: 0,
            winners: BTreeMap::new(),
            settled_sources: BTreeSet::new(),
            absent_emitted: BTreeSet::new(),
        }
    }

    pub fn sources(&self) -> &BTreeSet<RelayUrl> {
        &self.sources
    }

    pub fn authors(&self) -> &BTreeSet<PublicKey> {
        &self.authors
    }

    /// Re-root over the current generic provider needs. An unchanged set does
    /// not reopen. Zero sources opens no query and changes no author fact.
    pub fn reroot(&mut self, authors: BTreeSet<PublicKey>) -> Option<CoordinatorQuery> {
        if authors == self.authors {
            return None;
        }
        self.authors = authors;
        self.revision = self.revision.saturating_add(1);
        self.winners
            .retain(|author, _| self.authors.contains(author));
        self.settled_sources.clear();
        self.absent_emitted.clear();
        relay_list_demand(&self.authors, &self.sources).map(|demand| CoordinatorQuery {
            revision: self.revision,
            demand,
        })
    }

    /// Select canonical replaceable winners and parse them.
    pub fn observe(&mut self, events: impl IntoIterator<Item = Event>) -> Vec<CoordinatorUpdate> {
        self.observe_current_delta([], events)
    }

    /// Apply one atomic delta from the authoritative current-row projection.
    ///
    /// Removals are applied before additions irrespective of delivery order.
    /// This lets a replaceable winner's removal reveal an older current row
    /// in the same batch without emitting a transient absence. A removed
    /// winner with no replacement is forgotten immediately; if every exact
    /// source has already settled, that cleared author becomes
    /// [`CoordinatorUpdate::Absent`].
    pub fn observe_current_delta(
        &mut self,
        removed: impl IntoIterator<Item = EventId>,
        events: impl IntoIterator<Item = Event>,
    ) -> Vec<CoordinatorUpdate> {
        let removed: BTreeSet<EventId> = removed.into_iter().collect();
        let mut changed = BTreeSet::new();
        self.winners.retain(|author, event| {
            let keep = !removed.contains(&event.id);
            if !keep {
                changed.insert(*author);
            }
            keep
        });

        for event in events {
            if event.kind.as_u16() != RELAY_LIST_KIND || !self.authors.contains(&event.pubkey) {
                continue;
            }
            let wins = self
                .winners
                .get(&event.pubkey)
                .is_none_or(|current| candidate_wins(&event, current));
            if !wins {
                continue;
            }
            changed.insert(event.pubkey);
            self.winners.insert(event.pubkey, event);
        }

        changed
            .into_iter()
            .filter_map(|author| {
                if let Some(event) = self.winners.get(&author) {
                    self.absent_emitted.remove(&author);
                    return Some(CoordinatorUpdate::Present {
                        author,
                        routes: parse_relay_list(event),
                    });
                }
                (self.settled_sources.len() == self.sources.len()
                    && self.absent_emitted.insert(author))
                .then_some(CoordinatorUpdate::Absent { author })
            })
            .collect()
    }

    /// Consume a settlement of the exact current request. One-of-N sources,
    /// stale revisions, and undeclared relays settle nothing.
    pub fn settle(&mut self, revision: u64, relay: &RelayUrl) -> Vec<CoordinatorUpdate> {
        if revision != self.revision || !self.sources.contains(relay) {
            return Vec::new();
        }
        self.settled_sources.insert(relay.clone());
        if self.settled_sources.len() != self.sources.len() {
            return Vec::new();
        }
        let absent: Vec<PublicKey> = self
            .authors
            .iter()
            .filter(|author| {
                !self.winners.contains_key(author) && !self.absent_emitted.contains(author)
            })
            .copied()
            .collect();
        absent
            .into_iter()
            .map(|author| {
                self.absent_emitted.insert(author);
                CoordinatorUpdate::Absent { author }
            })
            .collect()
    }
}

fn candidate_wins(candidate: &Event, current: &Event) -> bool {
    candidate.created_at > current.created_at
        || (candidate.created_at == current.created_at && candidate.id < current.id)
}

#[cfg(test)]
mod tests {
    use nostr::{Keys, Timestamp};

    use super::*;

    fn relay(name: &str) -> RelayUrl {
        RelayUrl::parse(&format!("wss://{name}.example")).unwrap()
    }

    fn entry(name: &str, usage: RelayUsage) -> RelayListEntry {
        RelayListEntry::new(relay(name), usage)
    }

    fn relay_list_event(keys: &Keys, created_at: u64, rows: &[(&str, Option<&str>)]) -> Event {
        let tags = rows.iter().map(|(url, marker)| {
            let mut row = vec!["r".to_string(), (*url).to_string()];
            if let Some(marker) = marker {
                row.push((*marker).to_string());
            }
            Tag::parse(row).unwrap()
        });
        nostr::EventBuilder::new(Kind::RelayList, "")
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn request_rejects_empty_oversized_duplicate_and_read_only_shapes() {
        let author = Keys::generate().public_key();
        assert_eq!(
            BootstrapRelayList::new(author, vec![], vec![entry("write", RelayUsage::Write)]),
            Err(BootstrapRelayListError::EmptyBootstrapRelays)
        );

        let too_many = (0..=MAX_BOOTSTRAP_RELAYS)
            .map(|index| relay(&format!("bootstrap-{index}")))
            .collect();
        assert_eq!(
            BootstrapRelayList::new(author, too_many, vec![entry("write", RelayUsage::Write)]),
            Err(BootstrapRelayListError::TooManyBootstrapRelays {
                actual: MAX_BOOTSTRAP_RELAYS + 1,
                max: MAX_BOOTSTRAP_RELAYS,
            })
        );

        let duplicate = relay("duplicate");
        assert_eq!(
            BootstrapRelayList::new(
                author,
                vec![duplicate.clone(), duplicate.clone()],
                vec![entry("write", RelayUsage::Write)]
            ),
            Err(BootstrapRelayListError::DuplicateBootstrapRelay { relay: duplicate })
        );

        let advertised = relay("advertised");
        assert_eq!(
            BootstrapRelayList::new(
                author,
                vec![relay("bootstrap")],
                vec![
                    RelayListEntry::new(advertised.clone(), RelayUsage::Read),
                    RelayListEntry::new(advertised.clone(), RelayUsage::Write),
                ]
            ),
            Err(BootstrapRelayListError::DuplicateRelayListRelay { relay: advertised })
        );

        assert_eq!(
            BootstrapRelayList::new(author, vec![relay("bootstrap")], vec![]),
            Err(BootstrapRelayListError::EmptyRelayList)
        );

        let too_many_advertised = (0..=MAX_RELAY_LIST_ENTRIES)
            .map(|index| entry(&format!("advertised-{index}"), RelayUsage::Write))
            .collect();
        assert_eq!(
            BootstrapRelayList::new(author, vec![relay("bootstrap")], too_many_advertised),
            Err(BootstrapRelayListError::TooManyRelayListEntries {
                actual: MAX_RELAY_LIST_ENTRIES + 1,
                max: MAX_RELAY_LIST_ENTRIES,
            })
        );

        assert_eq!(
            BootstrapRelayList::new(
                author,
                vec![relay("bootstrap")],
                vec![entry("read", RelayUsage::Read)]
            ),
            Err(BootstrapRelayListError::NoWriteCapableRelay)
        );
    }

    #[test]
    fn fixed_time_composition_owns_exact_kind_tags_content_and_route() {
        let author = Keys::generate().public_key();
        let bootstrap_a = relay("bootstrap-a");
        let bootstrap_b = relay("bootstrap-b");
        let read_write = relay("read-write");
        let read = relay("read");
        let write = relay("write");
        let request = BootstrapRelayList::new(
            author,
            vec![bootstrap_b.clone(), bootstrap_a.clone()],
            vec![
                RelayListEntry::new(read_write.clone(), RelayUsage::ReadWrite),
                RelayListEntry::new(read.clone(), RelayUsage::Read),
                RelayListEntry::new(write.clone(), RelayUsage::Write),
            ],
        )
        .unwrap();

        let intent = request.into_write_intent();
        assert_eq!(intent.identity, Identity::Explicit(author));
        let WritePayload::Event(builder) = &intent.payload else {
            panic!("bootstrap must compose one builder")
        };
        assert_eq!(builder.created_at, None);
        assert_eq!(builder.kind, Kind::RelayList);
        assert_eq!(builder.content, "");
        assert_eq!(
            builder
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect::<Vec<_>>(),
            vec![
                vec!["r".to_string(), read_write.to_string()],
                vec!["r".to_string(), read.to_string(), "read".to_string()],
                vec!["r".to_string(), write.to_string(), "write".to_string()],
            ]
        );
        let WriteRouting::Explicit(relays) = intent.routing else {
            panic!("bootstrap mints an exact relay set, like any other protocol crate")
        };
        assert_eq!(relays, vec![bootstrap_a, bootstrap_b]);
    }

    #[test]
    fn parser_preserves_unmarked_read_write_and_drops_only_unreadable_rows() {
        let keys = Keys::generate();
        let both = relay("both");
        let read = relay("read");
        let write = relay("write");
        let event = relay_list_event(
            &keys,
            1,
            &[
                (both.as_str(), None),
                (read.as_str(), Some("read")),
                (write.as_str(), Some("write")),
                ("not a relay", None),
                ("wss://ignored.example", Some("future-marker")),
            ],
        );
        let routes = parse_relay_list(&event);
        assert_eq!(routes.outbound, BTreeSet::from([both.clone(), write]));
        assert_eq!(routes.inbound, BTreeSet::from([both, read]));
    }

    /// The parser holds one event and no identity, so it cannot know whose
    /// declaration this is. It must therefore report every readable row the
    /// author declared and refuse nothing — the defect it replaces is a local
    /// row vanishing here, where the loss is unrecoverable and unattributable
    /// (#1251).
    #[test]
    fn the_parser_reports_local_rows_instead_of_deciding_about_them() {
        let keys = Keys::generate();
        let loopback = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        let lan = RelayUrl::parse("ws://192.168.1.10").unwrap();
        let hidden = RelayUrl::parse("ws://nmprelayxyz.onion").unwrap();
        let event = relay_list_event(
            &keys,
            1,
            &[
                (loopback.as_str(), Some("write")),
                (lan.as_str(), None),
                (hidden.as_str(), Some("read")),
            ],
        );
        let routes = parse_relay_list(&event);
        assert_eq!(
            routes.outbound,
            BTreeSet::from([loopback, lan.clone()]),
            "every declared write row survives parsing"
        );
        assert_eq!(routes.inbound, BTreeSet::from([lan, hidden]));
    }

    #[test]
    fn coordinator_selects_winner_before_parse_and_settles_only_all_sources() {
        let keys = Keys::generate();
        let author = keys.public_key();
        let source_a = relay("source-a");
        let source_b = relay("source-b");
        let mut coordinator = Nip65Coordinator::new([source_a.clone(), source_b.clone()]);
        let query = coordinator
            .reroot(BTreeSet::from([author]))
            .expect("new need opens exact query");
        assert_eq!(
            query.demand.routing,
            ReadRouting::Explicit(vec![source_a.clone(), source_b.clone()])
        );

        let newer = relay_list_event(&keys, 2, &[("wss://new.example", None)]);
        let older = relay_list_event(&keys, 1, &[("wss://old.example", None)]);
        let updates = coordinator.observe([newer, older]);
        assert_eq!(updates.len(), 1, "older arrival cannot overwrite winner");
        let CoordinatorUpdate::Present { routes, .. } = &updates[0] else {
            panic!("positive winner")
        };
        assert_eq!(
            routes.outbound,
            BTreeSet::from([RelayUrl::parse("wss://new.example").unwrap()])
        );
        assert!(coordinator.settle(query.revision, &source_a).is_empty());
        assert!(
            coordinator.settle(query.revision, &source_b).is_empty(),
            "a winner prevents absence"
        );
    }

    #[test]
    fn zero_sources_and_stale_or_partial_settlement_never_mint_absence() {
        let author = Keys::generate().public_key();
        let mut zero = Nip65Coordinator::new([]);
        assert!(zero.reroot(BTreeSet::from([author])).is_none());

        let source_a = relay("source-a");
        let source_b = relay("source-b");
        let mut coordinator = Nip65Coordinator::new([source_a.clone(), source_b.clone()]);
        let query = coordinator.reroot(BTreeSet::from([author])).expect("query");
        assert!(coordinator
            .settle(query.revision.saturating_sub(1), &source_a)
            .is_empty());
        assert!(coordinator.settle(query.revision, &source_a).is_empty());
        assert_eq!(
            coordinator.settle(query.revision, &source_b),
            vec![CoordinatorUpdate::Absent { author }]
        );
        assert!(
            coordinator.settle(query.revision, &source_b).is_empty(),
            "absence emits once"
        );
    }

    #[test]
    fn authoritative_winner_removal_reveals_predecessor_atomically() {
        let keys = Keys::generate();
        let author = keys.public_key();
        let source = relay("source");
        let mut coordinator = Nip65Coordinator::new([source.clone()]);
        let query = coordinator.reroot(BTreeSet::from([author])).expect("query");
        let older = relay_list_event(&keys, 1, &[("wss://old.example", None)]);
        let newer = relay_list_event(&keys, 2, &[("wss://new.example", None)]);

        assert_eq!(coordinator.observe([newer.clone()]).len(), 1);
        assert!(coordinator.settle(query.revision, &source).is_empty());

        let updates = coordinator.observe_current_delta([newer.id], [older.clone()]);
        assert_eq!(
            updates,
            vec![CoordinatorUpdate::Present {
                author,
                routes: ParsedAuthorRoutes {
                    outbound: BTreeSet::from([RelayUrl::parse("wss://old.example").unwrap()]),
                    inbound: BTreeSet::from([RelayUrl::parse("wss://old.example").unwrap()]),
                },
            }],
            "same-batch replacement must never expose transient absence"
        );
    }

    #[test]
    fn authoritative_winner_removal_clears_winner_and_settles_absent() {
        let keys = Keys::generate();
        let author = keys.public_key();
        let source = relay("source");
        let mut coordinator = Nip65Coordinator::new([source.clone()]);
        let query = coordinator.reroot(BTreeSet::from([author])).expect("query");
        let winner = relay_list_event(&keys, 2, &[("wss://winner.example", None)]);
        assert_eq!(coordinator.observe([winner.clone()]).len(), 1);
        assert!(coordinator.settle(query.revision, &source).is_empty());

        assert_eq!(
            coordinator.observe_current_delta([winner.id], []),
            vec![CoordinatorUpdate::Absent { author }]
        );

        let later = relay_list_event(&keys, 3, &[("wss://later.example", None)]);
        assert!(
            matches!(
                coordinator.observe([later]).as_slice(),
                [CoordinatorUpdate::Present { author: updated, .. }] if updated == &author
            ),
            "a later positive row must overwrite session-derived absence"
        );
    }

    #[test]
    fn removing_a_non_winner_does_not_clear_the_current_row() {
        let keys = Keys::generate();
        let author = keys.public_key();
        let mut coordinator = Nip65Coordinator::new([relay("source")]);
        coordinator.reroot(BTreeSet::from([author])).expect("query");
        let older = relay_list_event(&keys, 1, &[("wss://old.example", None)]);
        let newer = relay_list_event(&keys, 2, &[("wss://new.example", None)]);
        assert_eq!(coordinator.observe([newer.clone(), older.clone()]).len(), 1);

        assert!(
            coordinator.observe_current_delta([older.id], []).is_empty(),
            "a removed row that was never the winner cannot affect the fact"
        );
    }
}
