//! Protocol-owned NIP-65 account bootstrap.
//!
//! A brand-new author has no kind:10002 in NMP's relay directory, so an
//! ordinary [`nmp::WriteRouting::Auto`] publication correctly fails
//! closed. [`publish_relay_list_bootstrap`] publishes the author's first
//! relay list to a validated exact relay set instead — an
//! [`nmp::WriteRouting::Explicit`] minted by this crate, the same "protocol
//! crate mints an exact route" pattern any other crate uses, running through
//! the ordinary durable acceptance, signer, outbox, and tracked receipt
//! pipeline. No dedicated routing variant is involved, and none is needed.
//!
//! The operation never mutates NMP's relay directory and never inserts a
//! synthetic network row or provenance fact. The new kind:10002 becomes a
//! routing fact only after it returns through an ordinary relay subscription
//! and the existing network-ingest path selects it as the canonical
//! replaceable winner. Every later write uses ordinary
//! [`nmp::WriteRouting::Auto`].

use std::collections::BTreeSet;

use nmp::{
    CorrelationToken, Durability, Engine, EngineError, EventBuilder, Kind, PublicKey,
    ReceiptStream, RelayUrl, Tag, WriteIntent, WritePayload, WriteRouting,
};
use nostr::nips::nip65::RelayMetadata;

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
    correlation: Option<CorrelationToken>,
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
            correlation: None,
        })
    }

    /// Attach a caller-persisted correlation token for crash-safe receipt
    /// recovery. Token uniqueness remains the ordinary NMP caller contract.
    pub fn with_correlation(mut self, correlation: CorrelationToken) -> Self {
        self.correlation = Some(correlation);
        self
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
}

/// Refusals that occur before or while handing the ordinary write intent to
/// the engine. Signer, route, and relay outcomes after handoff remain normal
/// [`nmp::WriteStatus`] facts on the returned receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapRelayListError {
    EmptyBootstrapRelays,
    TooManyBootstrapRelays { actual: usize, max: usize },
    DuplicateBootstrapRelay { relay: RelayUrl },
    EmptyRelayList,
    TooManyRelayListEntries { actual: usize, max: usize },
    DuplicateRelayListRelay { relay: RelayUrl },
    NoWriteCapableRelay,
    Engine(EngineError),
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
            Self::Engine(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BootstrapRelayListError {}

impl From<EngineError> for BootstrapRelayListError {
    fn from(value: EngineError) -> Self {
        Self::Engine(value)
    }
}

/// Publish a brand-new account's first kind:10002 and return its ordinary
/// stable-id tracked receipt.
///
/// The request's author must equal the engine's active account. A mismatch is
/// deliberately reported by the normal receipt as a pre-acceptance
/// [`nmp::WriteStatus::Failed`] fact; this function neither restamps the author
/// nor installs a signer itself.
pub fn publish_relay_list_bootstrap(
    engine: &Engine,
    request: BootstrapRelayList,
) -> Result<ReceiptStream, BootstrapRelayListError> {
    engine
        .publish_tracked(compose_relay_list_bootstrap(request))
        .map_err(Into::into)
}

fn compose_relay_list_bootstrap(request: BootstrapRelayList) -> WriteIntent {
    let BootstrapRelayList {
        author,
        bootstrap_relays,
        relay_list,
        correlation,
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
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(bootstrap_relays.into_iter().collect()),
        // The request names the account this bootstrap is FOR, and a
        // builder has no author of its own, so that name is now what
        // selects the signing identity rather than something the engine
        // has to compare a stamped author against.
        identity_override: Some(author),
        correlation,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nmp::{EngineConfig, WriteStatus};
    use nostr::Keys;

    use super::*;

    fn relay(name: &str) -> RelayUrl {
        RelayUrl::parse(&format!("wss://{name}.example")).unwrap()
    }

    fn entry(name: &str, usage: RelayUsage) -> RelayListEntry {
        RelayListEntry::new(relay(name), usage)
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

        let intent = compose_relay_list_bootstrap(request);
        assert_eq!(intent.identity_override, Some(author));
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

    /// The request names the account the bootstrap is FOR, and that name is
    /// now what selects the signing identity -- so bootstrapping a DIFFERENT
    /// account than the active one is a legitimate write, not a mismatch.
    /// With no signer registered for it, it parks on that exact key rather
    /// than failing or drifting onto the active account.
    #[test]
    fn a_bootstrap_for_another_account_parks_on_that_exact_key() {
        let active = Keys::generate();
        let different = Keys::generate();
        let engine = Engine::new(EngineConfig::default()).unwrap();
        let _registration = engine
            .add_account(&active.secret_key().to_secret_hex())
            .unwrap();
        engine
            .set_active_account(Some(active.public_key()))
            .unwrap();
        let request = BootstrapRelayList::new(
            different.public_key(),
            vec![relay("bootstrap")],
            vec![entry("write", RelayUsage::Write)],
        )
        .unwrap();

        let receipt = publish_relay_list_bootstrap(&engine, request).unwrap();
        let mut awaited = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match receipt.statuses.recv_timeout(Duration::from_millis(100)) {
                Ok(WriteStatus::AwaitingCapability { pubkey }) => {
                    awaited = Some(pubkey);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert_eq!(awaited, Some(different.public_key()));
        engine.shutdown();
    }

    #[test]
    fn engine_shutdown_remains_a_typed_synchronous_handoff_failure() {
        let keys = Keys::generate();
        let engine = Engine::new(EngineConfig::default()).unwrap();
        engine.shutdown();
        let request = BootstrapRelayList::new(
            keys.public_key(),
            vec![relay("bootstrap")],
            vec![entry("write", RelayUsage::Write)],
        )
        .unwrap();

        let error = match publish_relay_list_bootstrap(&engine, request) {
            Ok(_) => panic!("a shut down engine must not return a receipt"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            BootstrapRelayListError::Engine(EngineError::EngineClosed)
        );
    }
}
