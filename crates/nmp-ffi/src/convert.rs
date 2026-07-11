//! `FfiFilter -> nmp_grammar::Filter` (and back, for the round-trip test)
//! plus `nostr::Event -> FfiRow`/`nmp_engine` value mirrors (M4 plan §2 step
//! A). Every parse of a foreign-supplied string (hex ids/keys, a tag-name
//! character, a relay URL) returns a typed [`FfiError`], never a panic --
//! errors are values across this boundary (plan §2/§6).

use std::collections::{BTreeMap, HashMap};

use nmp_engine::core::{
    DiagnosticsSnapshot, FilterCoverageEntry, QueryCoverage, RelayDiagnosticsSnapshot, RowDelta,
};
use nmp_engine::outbox::{
    Durability as GDurability, WriteIntent as GWriteIntent, WritePayload as GWritePayload,
    WriteRouting as GWriteRouting, WriteStatus as GWriteStatus,
};
use nmp_grammar::{
    Binding as GBinding, Derived as GDerived, Filter as GFilter, IdentityField as GIdentityField,
    IndexedTagName, Selector as GSelector, SetAlgebra as GSetAlgebra, SetOp as GSetOp,
};
use nmp_router::Lane;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event as SignedEvent, EventId, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

use crate::types::{
    FfiBinding, FfiCoverage, FfiDerived, FfiDiagnosticsSnapshot, FfiDurability, FfiFilter,
    FfiFilterCoverage, FfiIdentityField, FfiKindCount, FfiLaneCount, FfiRelayDiagnostics, FfiRow,
    FfiRowDelta, FfiSelector, FfiSetAlgebra, FfiSetOp, FfiWriteIntent, FfiWritePayload,
    FfiWriteRouting, FfiWriteStatus,
};

/// Every way a value crossing this boundary can fail to parse -- typed
/// states, never a panic (plan §2/§6).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiError {
    /// A `FfiFilter.tags` key was not exactly one ASCII letter (`a`-`z` or
    /// `A`-`Z`) -- the wire/local INDEXED filter alphabet (NIP-01
    /// `#<letter>` queries). This is NOT a judgment that the string is a
    /// malformed event tag (see [`Self::InvalidTag`] for that) -- a
    /// multi-character or punctuation name is perfectly valid *event* data,
    /// it simply cannot be a generic filter key. `FfiSelector::Tag`'s `name`
    /// is never checked against this rule (#64).
    NonIndexableFilterTag {
        got: String,
    },
    InvalidPublicKey {
        got: String,
    },
    /// A `FfiBinding::Literal` value in the `ids` field position was not a
    /// valid 32-byte-hex event id.
    InvalidEventId {
        got: String,
    },
    InvalidRelayUrl {
        got: String,
    },
    /// A raw `[String; N]` tag in a `FfiWriteIntent` did not parse as a
    /// valid nostr tag (`Tag::parse`) -- e.g. an empty array. Rejecting the
    /// whole intent here (rather than silently dropping the malformed tag)
    /// is what keeps the signed event identical to what the app composed.
    InvalidTag {
        got: Vec<String>,
    },
    /// `add_account`'s secret key did not parse as a valid nostr key (hex or
    /// bech32 `nsec`).
    InvalidSecretKey,
    /// A registered signing capability reported no public key at all --
    /// never true for `LocalKeySigner`, but the registry's own contract
    /// (`nmp_signer::SigningCapability::public_key() -> Option<PublicKey>`)
    /// allows it, so this stays a typed state rather than an assumption.
    SignerHasNoPublicKey,
    /// `NmpEngine::new`'s `store_path` pointed at a file `RedbStore::open`
    /// could not open.
    StoreOpenFailed {
        reason: String,
    },
    /// A `FfiWritePayload::Signed`'s `sig` did not parse as a valid 64-byte
    /// hex schnorr signature.
    InvalidSignature {
        got: String,
    },
    /// A `FfiWritePayload::Signed` whose fields parsed but did not pass
    /// `nostr::Event::verify` -- the id does not hash to these fields, or
    /// the signature does not verify against `pubkey`. Rejected HERE (#32):
    /// the engine never sees, and never publishes, this event.
    InvalidSignedEvent {
        reason: String,
    },
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonIndexableFilterTag { got } => {
                write!(f, "not indexable as a filter key: {got:?}")
            }
            Self::InvalidPublicKey { got } => write!(f, "invalid public key hex: {got:?}"),
            Self::InvalidEventId { got } => write!(f, "invalid event id hex: {got:?}"),
            Self::InvalidRelayUrl { got } => write!(f, "invalid relay url: {got:?}"),
            Self::InvalidTag { got } => write!(f, "invalid tag: {got:?}"),
            Self::InvalidSecretKey => write!(f, "invalid secret key"),
            Self::SignerHasNoPublicKey => write!(f, "signer reported no public key"),
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
            Self::InvalidSignature { got } => write!(f, "invalid signature hex: {got:?}"),
            Self::InvalidSignedEvent { reason } => write!(f, "invalid signed event: {reason}"),
        }
    }
}

impl std::error::Error for FfiError {}

/// Parse an `FfiFilter.tags` key -- the wire/local INDEXED filter alphabet
/// only. Exactly one ASCII letter (`a`-`z`/`A`-`Z`) is accepted; anything
/// else (empty, multi-character, digit, punctuation) fails with a typed
/// [`FfiError::NonIndexableFilterTag`], never a whitelist rejection. This is
/// NOT used for `FfiSelector::Tag`'s `name` -- that is an arbitrary
/// event-tag key and passes through unchecked (#64).
pub fn indexed_tag_name_from_ffi(s: &str) -> Result<IndexedTagName, FfiError> {
    let mut chars = s.chars();
    let only = chars.next();
    match (only, chars.next()) {
        (Some(c), None) => IndexedTagName::new(c)
            .ok_or_else(|| FfiError::NonIndexableFilterTag { got: s.to_string() }),
        _ => Err(FfiError::NonIndexableFilterTag { got: s.to_string() }),
    }
}

fn identity_field_from_ffi(f: FfiIdentityField) -> GIdentityField {
    match f {
        FfiIdentityField::ActivePubkey => GIdentityField::ActivePubkey,
    }
}

fn identity_field_to_ffi(f: GIdentityField) -> FfiIdentityField {
    match f {
        GIdentityField::ActivePubkey => FfiIdentityField::ActivePubkey,
    }
}

fn selector_from_ffi(s: FfiSelector) -> Result<GSelector, FfiError> {
    Ok(match s {
        FfiSelector::Authors => GSelector::Authors,
        FfiSelector::Ids => GSelector::Ids,
        // Arbitrary event-tag key (#64) -- NOT run through
        // `indexed_tag_name_from_ffi`. Selector::Tag projects already-
        // acquired events locally; it never inherits the wire filter's
        // single-letter restriction, so every string is accepted verbatim.
        FfiSelector::Tag { name } => GSelector::Tag(name),
        FfiSelector::AddressCoord => GSelector::AddressCoord,
    })
}

fn selector_to_ffi(s: GSelector) -> FfiSelector {
    match s {
        GSelector::Authors => FfiSelector::Authors,
        GSelector::Ids => FfiSelector::Ids,
        GSelector::Tag(name) => FfiSelector::Tag { name },
        GSelector::AddressCoord => FfiSelector::AddressCoord,
    }
}

fn set_algebra_from_ffi(a: FfiSetAlgebra) -> GSetAlgebra {
    match a {
        FfiSetAlgebra::Union => GSetAlgebra::Union,
        FfiSetAlgebra::Intersect => GSetAlgebra::Intersect,
        FfiSetAlgebra::Diff => GSetAlgebra::Diff,
    }
}

fn set_algebra_to_ffi(a: GSetAlgebra) -> FfiSetAlgebra {
    match a {
        GSetAlgebra::Union => FfiSetAlgebra::Union,
        GSetAlgebra::Intersect => FfiSetAlgebra::Intersect,
        GSetAlgebra::Diff => FfiSetAlgebra::Diff,
    }
}

/// Which field a [`FfiBinding::Literal`] is being parsed for -- `authors`
/// and `ids` carry a hex-encoding invariant that `ConcreteFilter::to_nostr`
/// (nmp-grammar) later PANICS on if violated (its own doc: "a genuine
/// invariant violation upstream, not a reachable user input error"). This
/// boundary is exactly that upstream: a foreign-supplied `Literal` string is
/// unchecked until here, so an FFI caller passing a bad hex string must get
/// a typed [`FfiError`], never let the panic fire two crates downstream.
/// Tag values have no such invariant (`to_nostr` never parses them as
/// hex) so `Tag` values pass through unchecked, same as before.
#[derive(Clone, Copy)]
enum LiteralField {
    Authors,
    Ids,
    Tag,
}

fn validate_literal(field: LiteralField, value: String) -> Result<String, FfiError> {
    match field {
        LiteralField::Authors => {
            parse_pubkey(&value)?;
            Ok(value)
        }
        LiteralField::Ids => {
            nostr::EventId::from_hex(&value)
                .map_err(|_| FfiError::InvalidEventId { got: value.clone() })?;
            Ok(value)
        }
        LiteralField::Tag => Ok(value),
    }
}

fn binding_from_ffi(b: FfiBinding, field: LiteralField) -> Result<GBinding, FfiError> {
    Ok(match b {
        FfiBinding::Literal { values } => GBinding::Literal(
            values
                .into_iter()
                .map(|v| validate_literal(field, v))
                .collect::<Result<_, _>>()?,
        ),
        FfiBinding::Reactive { field: id_field } => {
            GBinding::Reactive(identity_field_from_ffi(id_field))
        }
        FfiBinding::Derived { derived } => GBinding::Derived(Box::new(GDerived {
            inner: filter_from_ffi(derived.inner.clone())?,
            project: selector_from_ffi(derived.project.clone())?,
        })),
        FfiBinding::SetOp { set_op } => GBinding::SetOp(Box::new(GSetOp {
            op: set_algebra_from_ffi(set_op.op),
            operands: set_op
                .operands
                .iter()
                .cloned()
                .map(|op| binding_from_ffi(op, field))
                .collect::<Result<_, _>>()?,
        })),
    })
}

pub fn binding_to_ffi(b: GBinding) -> FfiBinding {
    match b {
        GBinding::Literal(values) => FfiBinding::Literal {
            values: values.into_iter().collect(),
        },
        GBinding::Reactive(f) => FfiBinding::Reactive {
            field: identity_field_to_ffi(f),
        },
        GBinding::Derived(d) => FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                inner: filter_to_ffi(d.inner),
                project: selector_to_ffi(d.project),
            }),
        },
        GBinding::SetOp(s) => FfiBinding::SetOp {
            set_op: std::sync::Arc::new(FfiSetOp {
                op: set_algebra_to_ffi(s.op),
                operands: s.operands.into_iter().map(binding_to_ffi).collect(),
            }),
        },
    }
}

pub fn filter_from_ffi(f: FfiFilter) -> Result<GFilter, FfiError> {
    let mut tags = BTreeMap::new();
    for (k, v) in f.tags {
        tags.insert(
            indexed_tag_name_from_ffi(&k)?,
            binding_from_ffi(v, LiteralField::Tag)?,
        );
    }
    Ok(GFilter {
        kinds: f.kinds.map(|ks| ks.into_iter().collect()),
        authors: f
            .authors
            .map(|b| binding_from_ffi(b, LiteralField::Authors))
            .transpose()?,
        ids: f
            .ids
            .map(|b| binding_from_ffi(b, LiteralField::Ids))
            .transpose()?,
        tags,
        since: f.since,
        until: f.until,
        limit: f.limit.map(|l| l as usize),
    })
}

pub fn filter_to_ffi(f: GFilter) -> FfiFilter {
    FfiFilter {
        kinds: f.kinds.map(|ks| ks.into_iter().collect()),
        authors: f.authors.map(binding_to_ffi),
        ids: f.ids.map(binding_to_ffi),
        tags: f
            .tags
            .into_iter()
            .map(|(k, v)| (k.as_char().to_string(), binding_to_ffi(v)))
            .collect::<HashMap<_, _>>(),
        since: f.since,
        until: f.until,
        limit: f.limit.map(|l| l as u32),
    }
}

/// Raw tokens only (ledger #12) -- no formatted field is ever built here.
pub fn event_to_ffi_row(e: &nostr::Event) -> FfiRow {
    FfiRow {
        id: e.id.to_hex(),
        pubkey: e.pubkey.to_hex(),
        created_at: e.created_at.as_secs(),
        kind: e.kind.as_u16(),
        tags: e.tags.iter().map(|t| t.clone().to_vec()).collect(),
        content: e.content.clone(),
        sig: e.sig.to_string(),
    }
}

pub fn row_delta_to_ffi(d: &RowDelta) -> FfiRowDelta {
    match d {
        RowDelta::Added(event) => FfiRowDelta::Added {
            row: event_to_ffi_row(event),
        },
        RowDelta::Removed(id) => FfiRowDelta::Removed { id: id.to_hex() },
    }
}

pub fn coverage_to_ffi(c: QueryCoverage) -> FfiCoverage {
    match c {
        QueryCoverage::CompleteUpTo(ts) => FfiCoverage::CompleteUpTo {
            unix_seconds: ts.as_secs(),
        },
        QueryCoverage::Unknown => FfiCoverage::Unknown,
    }
}

pub fn write_status_to_ffi(s: WriteStatusRef<'_>) -> FfiWriteStatus {
    match s.0 {
        GWriteStatus::Accepted => FfiWriteStatus::Accepted,
        GWriteStatus::AwaitingCapability => FfiWriteStatus::AwaitingCapability,
        GWriteStatus::Signed(id) => FfiWriteStatus::Signed {
            event_id: id.to_hex(),
        },
        GWriteStatus::Routed(relays) => FfiWriteStatus::Routed {
            relays: relays.iter().map(RelayUrl::to_string).collect(),
        },
        GWriteStatus::Sent(relay) => FfiWriteStatus::Sent {
            relay: relay.to_string(),
        },
        GWriteStatus::Acked(relay) => FfiWriteStatus::Acked {
            relay: relay.to_string(),
        },
        GWriteStatus::Rejected(relay, reason) => FfiWriteStatus::Rejected {
            relay: relay.to_string(),
            reason: reason.clone(),
        },
        GWriteStatus::GaveUp(relay) => FfiWriteStatus::GaveUp {
            relay: relay.to_string(),
        },
        GWriteStatus::Failed(reason) => FfiWriteStatus::Failed {
            reason: reason.clone(),
        },
    }
}

/// `nmp_router::Lane` -> a stable string label (M5 plan §1.1). Rendered as a
/// string rather than an `Enum` mirror because the diagnostics screen only
/// ever displays it -- there is no round-trip/construction need the way
/// `FfiSelector`/`FfiBinding` have for the filter grammar.
fn lane_to_ffi_string(lane: Lane) -> String {
    match lane {
        Lane::Nip65Write => "nip65_write",
        Lane::Hint => "hint",
        Lane::Provenance => "provenance",
        Lane::UserConfigured => "user_configured",
        Lane::IndexerDiscovery => "indexer_discovery",
        Lane::GroupHost => "group_host",
        Lane::DmInbox => "dm_inbox",
        Lane::Nip65Read => "nip65_read",
        Lane::AppRelay => "app_relay",
        Lane::Fallback => "fallback",
    }
    .to_string()
}

fn relay_diagnostics_to_ffi(r: RelayDiagnosticsSnapshot) -> FfiRelayDiagnostics {
    FfiRelayDiagnostics {
        relay: r.relay.to_string(),
        wire_sub_count: r.wire_sub_count as u32,
        authors_served: r.authors_served as u32,
        by_lane: r
            .by_lane
            .into_iter()
            .map(|(lane, count)| FfiLaneCount {
                lane: lane_to_ffi_string(lane),
                count: count as u32,
            })
            .collect(),
        filters: r.filters,
        events_by_kind: r
            .events_by_kind
            .into_iter()
            .map(|(kind, count)| FfiKindCount { kind, count })
            .collect(),
        coverage: r
            .coverage
            .into_iter()
            .map(|entry: FilterCoverageEntry| FfiFilterCoverage {
                filter: entry.filter,
                coverage: coverage_to_ffi(entry.coverage),
            })
            .collect(),
    }
}

/// `nmp_engine::core::DiagnosticsSnapshot -> FfiDiagnosticsSnapshot` (M5 plan
/// §1.2 step 5) -- the engine-global diagnostics projection, rendered whole
/// for the FFI boundary. Every number/string here is copied straight off the
/// engine-owned snapshot, never recomputed/estimated at this layer.
pub fn diagnostics_snapshot_to_ffi(s: DiagnosticsSnapshot) -> FfiDiagnosticsSnapshot {
    FfiDiagnosticsSnapshot {
        relays: s.relays.into_iter().map(relay_diagnostics_to_ffi).collect(),
        uncovered_author_count: s.uncovered_author_count as u32,
        dropped_merge_rules: s
            .dropped_merge_rules
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

/// Newtype wrapper so `write_status_to_ffi` can take `&WriteStatus` without
/// this crate needing a `From<&WriteStatus>` orphan impl.
pub struct WriteStatusRef<'a>(pub &'a GWriteStatus);

pub fn parse_pubkey(hex: &str) -> Result<PublicKey, FfiError> {
    PublicKey::from_hex(hex).map_err(|_| FfiError::InvalidPublicKey {
        got: hex.to_string(),
    })
}

pub fn parse_relay_url(url: &str) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(url).map_err(|_| FfiError::InvalidRelayUrl {
        got: url.to_string(),
    })
}

/// A malformed raw tag array (empty, or otherwise unparseable) REJECTS the
/// whole intent rather than being silently dropped: a signer that drops one
/// tag from a template can sign a DIFFERENT event than the app composed
/// (e.g. a reply losing its `e` tag becomes a root note) -- exactly the
/// tag-integrity hole `filter_map(...).ok()` used to open. Every tag either
/// parses or the whole `write_intent_from_ffi` call fails closed with a
/// typed [`FfiError::InvalidTag`] naming the offending raw tag.
fn tags_from_ffi(tags: Vec<Vec<String>>) -> Result<Vec<Tag>, FfiError> {
    tags.into_iter()
        .map(|t| Tag::parse(t.clone()).map_err(|_| FfiError::InvalidTag { got: t }))
        .collect()
}

/// A `FfiWritePayload::Signed`'s fields -> a verified `nostr::Event`. Every
/// field is parsed with the same typed-error discipline as the rest of this
/// module, then the reconstructed event is run through `Event::verify`
/// (id + schnorr signature, the same capability `nmp-transport`'s ingest
/// gate reuses) -- a malformed or non-verifying event never reaches
/// `write_intent_from_ffi`'s caller as anything but a typed [`FfiError`],
/// so `nmp-engine` can never be handed garbage to publish verbatim (#32).
fn signed_event_from_ffi(
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
) -> Result<SignedEvent, FfiError> {
    let event_id = EventId::from_hex(&id).map_err(|_| FfiError::InvalidEventId { got: id })?;
    let public_key = parse_pubkey(&pubkey)?;
    let parsed_tags = tags_from_ffi(tags)?;
    let signature = sig
        .parse::<Signature>()
        .map_err(|_| FfiError::InvalidSignature { got: sig })?;

    let event = SignedEvent::new(
        event_id,
        public_key,
        Timestamp::from(created_at),
        nostr::Kind::from(kind),
        parsed_tags,
        content,
        signature,
    );
    event.verify().map_err(|e| FfiError::InvalidSignedEvent {
        reason: e.to_string(),
    })?;
    Ok(event)
}

/// `FfiWriteIntent -> nmp_engine::outbox::WriteIntent`. `Unsigned` builds an
/// `UnsignedEvent` template the engine signs internally; `Signed` (#32)
/// verifies the caller-supplied event and passes it through verbatim -- see
/// `signed_event_from_ffi`.
pub fn write_intent_from_ffi(intent: FfiWriteIntent) -> Result<GWriteIntent, FfiError> {
    let payload = match intent.payload {
        FfiWritePayload::Unsigned {
            pubkey,
            created_at,
            kind,
            tags,
            content,
        } => {
            let pubkey = parse_pubkey(&pubkey)?;
            let unsigned = UnsignedEvent::new(
                pubkey,
                Timestamp::from(created_at),
                nostr::Kind::from(kind),
                tags_from_ffi(tags)?,
                content,
            );
            GWritePayload::Unsigned(unsigned)
        }
        FfiWritePayload::Signed {
            id,
            pubkey,
            created_at,
            kind,
            tags,
            content,
            sig,
        } => {
            let event = signed_event_from_ffi(id, pubkey, created_at, kind, tags, content, sig)?;
            GWritePayload::Signed(event)
        }
    };

    let durability = match intent.durability {
        FfiDurability::Durable => GDurability::Durable,
        FfiDurability::Ephemeral => GDurability::Ephemeral,
        FfiDurability::AtMostOnce => GDurability::AtMostOnce,
    };

    let routing = match intent.routing {
        FfiWriteRouting::AuthorOutbox => GWriteRouting::AuthorOutbox,
        FfiWriteRouting::ToInboxes { recipients } => {
            let pks = recipients
                .iter()
                .map(|hex| parse_pubkey(hex))
                .collect::<Result<Vec<_>, _>>()?;
            GWriteRouting::ToInboxes(pks)
        }
        FfiWriteRouting::PrivateNarrow { relays } => {
            let urls = relays
                .iter()
                .map(|u| parse_relay_url(u))
                .collect::<Result<Vec<_>, _>>()?;
            GWriteRouting::PrivateNarrow(nmp_engine::outbox::PrivateRoute {
                relays: nmp_engine::outbox::NarrowOnly::new(urls),
            })
        }
    };

    Ok(GWriteIntent {
        payload,
        durability,
        routing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiIdentityField;

    fn pk_hex() -> String {
        "a".repeat(64)
    }

    #[test]
    fn literal_binding_round_trips() {
        let ffi = FfiFilter {
            kinds: Some(vec![1]),
            authors: Some(FfiBinding::Literal {
                values: vec![pk_hex()],
            }),
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("valid filter");
        let back = filter_to_ffi(grammar);
        assert_eq!(ffi, back);
    }

    #[test]
    fn reactive_and_tag_binding_round_trips() {
        let mut tags = HashMap::new();
        tags.insert(
            "p".to_string(),
            FfiBinding::Reactive {
                field: FfiIdentityField::ActivePubkey,
            },
        );
        let ffi = FfiFilter {
            kinds: Some(vec![1]),
            tags,
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("valid filter");
        let back = filter_to_ffi(grammar);
        assert_eq!(ffi, back);
    }

    #[test]
    fn nip29_h_tag_binding_round_trips() {
        let mut tags = HashMap::new();
        tags.insert(
            "h".to_string(),
            FfiBinding::Literal {
                values: vec!["group-id".to_string()],
            },
        );
        let ffi = FfiFilter {
            kinds: Some(vec![9, 30_315]),
            tags,
            ..FfiFilter::default()
        };

        let grammar = filter_from_ffi(ffi.clone()).expect("h is a valid ASCII-letter tag key");
        assert_eq!(filter_to_ffi(grammar), ffi);
    }

    #[test]
    fn derived_and_set_op_round_trip() {
        let derived = FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                inner: FfiFilter {
                    kinds: Some(vec![3]),
                    authors: Some(FfiBinding::Reactive {
                        field: FfiIdentityField::ActivePubkey,
                    }),
                    ..FfiFilter::default()
                },
                project: FfiSelector::Tag {
                    name: "p".to_string(),
                },
            }),
        };
        let mutes = FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                inner: FfiFilter {
                    kinds: Some(vec![10_000]),
                    authors: Some(FfiBinding::Reactive {
                        field: FfiIdentityField::ActivePubkey,
                    }),
                    ..FfiFilter::default()
                },
                project: FfiSelector::Tag {
                    name: "p".to_string(),
                },
            }),
        };
        let ffi = FfiFilter {
            kinds: Some(vec![1]),
            authors: Some(FfiBinding::SetOp {
                set_op: std::sync::Arc::new(FfiSetOp {
                    op: FfiSetAlgebra::Diff,
                    operands: vec![derived, mutes],
                }),
            }),
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("valid filter");
        let back = filter_to_ffi(grammar);
        assert_eq!(ffi, back);
    }

    #[test]
    fn multi_character_filter_tag_key_is_a_typed_non_indexable_error_not_a_panic() {
        let mut tags = HashMap::new();
        tags.insert(
            "zz".to_string(),
            FfiBinding::Literal {
                values: vec![pk_hex()],
            },
        );
        let ffi = FfiFilter {
            tags,
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::NonIndexableFilterTag {
                got: "zz".to_string()
            })
        );
    }

    /// Every ASCII letter, both cases, is a valid `FfiFilter.tags` key --
    /// structural, not a hand-picked subset. `x`/`Z` in particular are NOT
    /// in the old hard-coded M1 whitelist; round-tripping them here proves
    /// the fix is syntax-based, not another expanded list (#64 acceptance
    /// evidence).
    #[test]
    fn every_ascii_letter_is_a_valid_filter_tag_key_round_trip() {
        for c in ('a'..='z').chain('A'..='Z') {
            let mut tags = HashMap::new();
            tags.insert(
                c.to_string(),
                FfiBinding::Literal {
                    values: vec!["v".to_string()],
                },
            );
            let ffi = FfiFilter {
                tags,
                ..FfiFilter::default()
            };
            let grammar = filter_from_ffi(ffi.clone())
                .unwrap_or_else(|e| panic!("{c:?} must be a valid filter tag key: {e}"));
            assert_eq!(filter_to_ffi(grammar), ffi);
        }
    }

    /// `FfiSelector::Tag`'s `name` is an arbitrary event-tag key, never
    /// checked against the indexed-filter single-letter rule: `"-"`,
    /// `"poop"`, and `"alt"` must round-trip unchanged, not be rejected as
    /// "unknown" (#64 acceptance evidence).
    #[test]
    fn selector_tag_accepts_arbitrary_event_tag_names_unchecked() {
        for name in ["-", "poop", "alt"] {
            let ffi = FfiSelector::Tag {
                name: name.to_string(),
            };
            let grammar = selector_from_ffi(ffi.clone())
                .unwrap_or_else(|e| panic!("{name:?} must be a valid Selector::Tag key: {e}"));
            assert_eq!(grammar, GSelector::Tag(name.to_string()));
            assert_eq!(selector_to_ffi(grammar), ffi);
        }
    }

    /// The core regression test for the panic-turned-typed-error: a
    /// `Literal` value in the `authors` field position that is NOT valid
    /// hex used to sail through `binding_from_ffi` unchecked and only blow
    /// up later, as a PANIC, inside `ConcreteFilter::to_nostr` (nmp-grammar)
    /// -- two crates downstream of the actual bad input, and un-catchable
    /// by the caller. It must now fail AT THIS BOUNDARY with a typed error.
    #[test]
    fn invalid_literal_author_hex_is_a_typed_error_not_a_panic() {
        let ffi = FfiFilter {
            authors: Some(FfiBinding::Literal {
                values: vec!["not-valid-hex".to_string()],
            }),
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::InvalidPublicKey {
                got: "not-valid-hex".to_string()
            })
        );
    }

    /// Same invariant, `ids` field position (a distinct hex-decoding path
    /// in `ConcreteFilter::to_nostr` -- `EventId::from_hex`, not
    /// `PublicKey::from_hex` -- so it gets its own falsifier).
    #[test]
    fn invalid_literal_id_hex_is_a_typed_error_not_a_panic() {
        let ffi = FfiFilter {
            ids: Some(FfiBinding::Literal {
                values: vec!["also-not-hex".to_string()],
            }),
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::InvalidEventId {
                got: "also-not-hex".to_string()
            })
        );
    }

    /// A `Literal` nested inside a `SetOp` at the `authors` position must
    /// still be validated -- the field position propagates through
    /// `SetOp`'s operands, it isn't lost the moment a binding gets
    /// composite.
    #[test]
    fn invalid_literal_inside_set_op_authors_operand_is_a_typed_error() {
        let ffi = FfiFilter {
            authors: Some(FfiBinding::SetOp {
                set_op: std::sync::Arc::new(FfiSetOp {
                    op: FfiSetAlgebra::Union,
                    operands: vec![FfiBinding::Literal {
                        values: vec!["garbage".to_string()],
                    }],
                }),
            }),
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::InvalidPublicKey {
                got: "garbage".to_string()
            })
        );
    }

    /// Tag VALUES (as opposed to the tag NAME/key) carry no hex invariant
    /// downstream (`ConcreteFilter::to_nostr` never parses a tag value as
    /// hex) -- a non-hex `Literal` at a tag position must still round-trip,
    /// not be rejected by the new authors/ids validation.
    #[test]
    fn non_hex_literal_tag_value_is_still_accepted() {
        let mut tags = HashMap::new();
        tags.insert(
            "d".to_string(),
            FfiBinding::Literal {
                values: vec!["my-identifier-not-hex".to_string()],
            },
        );
        let ffi = FfiFilter {
            tags,
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("tag values need no hex validation");
        assert_eq!(filter_to_ffi(grammar), ffi);
    }

    fn valid_write_intent() -> FfiWriteIntent {
        FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: pk_hex(),
                created_at: 100,
                kind: 1,
                tags: vec![vec!["e".to_string(), "e".repeat(64)]],
                content: "hello".to_string(),
            },
            durability: FfiDurability::Ephemeral,
            routing: FfiWriteRouting::AuthorOutbox,
        }
    }

    #[test]
    fn well_formed_write_intent_parses_ok() {
        let intent = valid_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("well-formed intent must parse");
        match parsed.payload {
            GWritePayload::Unsigned(u) => assert_eq!(u.tags.len(), 1),
            GWritePayload::Signed(_) => {
                panic!("an Unsigned FfiWritePayload must build an Unsigned GWritePayload")
            }
        }
    }

    /// Arbitrary event tags survive the write boundary UNCHANGED and are
    /// never routed through indexed-key validation (#64 acceptance
    /// evidence / codex-nova review item 3): `"-"`/`"poop"`/`"alt"` are
    /// multi-character/punctuation tag NAMES that would fail
    /// `indexed_tag_name_from_ffi` (they are not filter keys at all here),
    /// yet `write_intent_from_ffi` must accept them verbatim -- raw tag
    /// arrays go through `tags_from_ffi`/`Tag::parse`, never
    /// `indexed_tag_name_from_ffi`.
    #[test]
    fn arbitrary_event_tags_survive_write_intent_from_ffi_unchanged() {
        let mut intent = valid_write_intent();
        let FfiWritePayload::Unsigned { tags, .. } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        *tags = vec![
            vec!["-".to_string()],
            vec!["poop".to_string(), "value".to_string()],
            vec!["alt".to_string(), "a human-readable summary".to_string()],
        ];
        let expected = tags.clone();

        let parsed = write_intent_from_ffi(intent)
            .expect("multi-character/punctuation event-tag names must not be rejected");
        let GWritePayload::Unsigned(unsigned) = parsed.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        let round_tripped: Vec<Vec<String>> =
            unsigned.tags.iter().map(|t| t.clone().to_vec()).collect();
        assert_eq!(
            round_tripped, expected,
            "raw tag arrays must survive write_intent_from_ffi byte-for-byte, \
             never normalized/rejected as unknown"
        );
    }

    /// The tag-integrity regression test: a malformed raw tag (here, an
    /// empty array -- `Tag::parse` rejects it) used to be silently DROPPED
    /// by `tags_from_ffi`'s `filter_map(...).ok()`, so the signed event
    /// would differ from what the app composed (e.g. a reply silently
    /// losing its `e` tag and becoming a root note). The whole intent must
    /// now fail closed with a typed error instead.
    #[test]
    fn malformed_tag_rejects_whole_write_intent_not_silently_dropped() {
        let mut intent = valid_write_intent();
        let FfiWritePayload::Unsigned { tags, .. } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        tags.push(Vec::new()); // empty tag array: Tag::parse rejects this
        match write_intent_from_ffi(intent) {
            Err(err) => assert_eq!(err, FfiError::InvalidTag { got: Vec::new() }),
            Ok(_) => panic!("a malformed tag must fail closed, not silently drop"),
        }
    }

    /// A real signed event (`EventBuilder::sign_with_keys`), rendered field-
    /// for-field into a `FfiWritePayload::Signed` the same way an app would
    /// after receiving one from an external signer / NIP-46 bunker.
    fn signed_write_intent() -> (nostr::Event, FfiWriteIntent) {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::TextNote, "presigned")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Signed {
                id: event.id.to_hex(),
                pubkey: event.pubkey.to_hex(),
                created_at: event.created_at.as_secs(),
                kind: event.kind.as_u16(),
                tags: event.tags.iter().map(|t| t.clone().to_vec()).collect(),
                content: event.content.clone(),
                sig: event.sig.to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
        };
        (event, intent)
    }

    /// #32's core contract: a pre-signed event round-trips to the engine's
    /// `WritePayload::Signed` byte-identical -- same id, same sig -- never
    /// re-derived.
    #[test]
    fn ffi_publishes_presigned_event_verbatim() {
        let (original, intent) = signed_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("a genuinely signed event must parse");
        match parsed.payload {
            GWritePayload::Signed(event) => {
                assert_eq!(event.id, original.id);
                assert_eq!(event.sig, original.sig);
                assert_eq!(event.pubkey, original.pubkey);
                assert_eq!(event.content, original.content);
            }
            GWritePayload::Unsigned(_) => {
                panic!("a Signed FfiWritePayload must build a Signed GWritePayload")
            }
        }
    }

    /// #32: the sign stage is a structural no-op for `Signed` -- there is no
    /// `UnsignedEvent` anywhere in the `Signed` arm to hand a signer, so this
    /// is falsified at the type level as much as the runtime one; this test
    /// pins the runtime half (the exact bytes handed in are the exact bytes
    /// that would reach `Effect::RequestSign` if this were mistakenly routed
    /// there -- it never is, per `on_publish`).
    #[test]
    fn ffi_presigned_never_resigned() {
        let (original, intent) = signed_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("a genuinely signed event must parse");
        let GWritePayload::Signed(event) = parsed.payload else {
            panic!("a Signed FfiWritePayload must build a Signed GWritePayload")
        };
        // A re-sign would mint a fresh id/sig; verbatim pass-through keeps
        // the caller's own id/sig, which only "same as original" can prove.
        assert_eq!(event.id, original.id);
        assert_eq!(event.sig, original.sig);
    }

    /// #32: a signature that does not verify against the claimed id/pubkey
    /// must fail closed with a typed error, never reach the engine.
    #[test]
    fn ffi_rejects_malformed_signed_event() {
        let (_original, mut intent) = signed_write_intent();
        let FfiWritePayload::Signed { content, .. } = &mut intent.payload else {
            unreachable!("signed_write_intent always builds Signed")
        };
        // Tamper with the content after signing: id/sig no longer match it.
        *content = "tampered".to_string();

        match write_intent_from_ffi(intent) {
            Err(FfiError::InvalidSignedEvent { .. }) => {}
            Err(other) => {
                panic!("expected InvalidSignedEvent, got a different FfiError: {other:?}")
            }
            Ok(_) => panic!("a tampered signed event must fail closed, not parse"),
        }
    }

    /// A `sig` that isn't even valid hex is a distinct, earlier failure mode
    /// from a well-formed-but-non-verifying signature.
    #[test]
    fn ffi_rejects_signed_event_with_unparseable_signature() {
        let (_original, mut intent) = signed_write_intent();
        let FfiWritePayload::Signed { sig, .. } = &mut intent.payload else {
            unreachable!("signed_write_intent always builds Signed")
        };
        *sig = "not-hex".to_string();

        match write_intent_from_ffi(intent) {
            Err(FfiError::InvalidSignature { got }) => assert_eq!(got, "not-hex"),
            Err(other) => panic!("expected InvalidSignature, got a different FfiError: {other:?}"),
            Ok(_) => panic!("an unparseable sig must fail closed, not parse"),
        }
    }
}
