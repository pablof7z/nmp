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
    Selector as GSelector, SetAlgebra as GSetAlgebra, SetOp as GSetOp, TagName,
};
use nmp_router::Lane;
use nostr::{PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

use crate::types::{
    FfiBinding, FfiCoverage, FfiDerived, FfiDiagnosticsSnapshot, FfiDurability, FfiFilter,
    FfiFilterCoverage, FfiIdentityField, FfiKindCount, FfiLaneCount, FfiRelayDiagnostics, FfiRow,
    FfiRowDelta, FfiSelector, FfiSetAlgebra, FfiSetOp, FfiWriteIntent, FfiWriteRouting,
    FfiWriteStatus,
};

/// Every way a value crossing this boundary can fail to parse -- typed
/// states, never a panic (plan §2/§6).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiError {
    /// A `FfiSelector::Tag`/`FfiFilter.tags` key was not exactly one
    /// character from the closed M1 set (`p, e, a, d, E, t, q`).
    InvalidTagName {
        got: String,
    },
    InvalidPublicKey {
        got: String,
    },
    InvalidRelayUrl {
        got: String,
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
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTagName { got } => write!(f, "invalid tag name: {got:?}"),
            Self::InvalidPublicKey { got } => write!(f, "invalid public key hex: {got:?}"),
            Self::InvalidRelayUrl { got } => write!(f, "invalid relay url: {got:?}"),
            Self::InvalidSecretKey => write!(f, "invalid secret key"),
            Self::SignerHasNoPublicKey => write!(f, "signer reported no public key"),
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
        }
    }
}

impl std::error::Error for FfiError {}

pub fn tag_name_from_ffi(s: &str) -> Result<TagName, FfiError> {
    let mut chars = s.chars();
    let only = chars.next();
    match (only, chars.next()) {
        (Some(c), None) => {
            TagName::new(c).ok_or_else(|| FfiError::InvalidTagName { got: s.to_string() })
        }
        _ => Err(FfiError::InvalidTagName { got: s.to_string() }),
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
        FfiSelector::Tag { name } => GSelector::Tag(tag_name_from_ffi(&name)?),
        FfiSelector::AddressCoord => GSelector::AddressCoord,
    })
}

fn selector_to_ffi(s: GSelector) -> FfiSelector {
    match s {
        GSelector::Authors => FfiSelector::Authors,
        GSelector::Ids => FfiSelector::Ids,
        GSelector::Tag(t) => FfiSelector::Tag {
            name: t.as_char().to_string(),
        },
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

pub fn binding_from_ffi(b: FfiBinding) -> Result<GBinding, FfiError> {
    Ok(match b {
        FfiBinding::Literal { values } => GBinding::Literal(values.into_iter().collect()),
        FfiBinding::Reactive { field } => GBinding::Reactive(identity_field_from_ffi(field)),
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
                .map(binding_from_ffi)
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
        tags.insert(tag_name_from_ffi(&k)?, binding_from_ffi(v)?);
    }
    Ok(GFilter {
        kinds: f.kinds.map(|ks| ks.into_iter().collect()),
        authors: f.authors.map(binding_from_ffi).transpose()?,
        ids: f.ids.map(binding_from_ffi).transpose()?,
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

fn tags_from_ffi(tags: Vec<Vec<String>>) -> Vec<Tag> {
    // A malformed raw tag array (empty, or otherwise unparseable) is simply
    // dropped rather than failing the whole publish -- the durable-write
    // contract (ledger #9) is about delivery, not template validation; a
    // template this malformed will fail as a signature/content mismatch
    // downstream if it matters, never silently corrupting an adjacent tag.
    tags.into_iter()
        .filter_map(|t| Tag::parse(t).ok())
        .collect()
}

/// `FfiWriteIntent -> nmp_engine::outbox::WriteIntent`. Always constructs an
/// `Unsigned` payload -- see `FfiWriteIntent`'s own doc for why `Signed` is
/// out of M4's FFI scope.
pub fn write_intent_from_ffi(intent: FfiWriteIntent) -> Result<GWriteIntent, FfiError> {
    let pubkey = parse_pubkey(&intent.pubkey)?;
    let unsigned = UnsignedEvent::new(
        pubkey,
        Timestamp::from(intent.created_at),
        nostr::Kind::from(intent.kind),
        tags_from_ffi(intent.tags),
        intent.content,
    );

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
        payload: GWritePayload::Unsigned(unsigned),
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
    fn invalid_tag_name_is_a_typed_error_not_a_panic() {
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
            Err(FfiError::InvalidTagName {
                got: "zz".to_string()
            })
        );
    }
}
