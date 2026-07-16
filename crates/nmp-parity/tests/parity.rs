//! #52 Unit D: execute one content-neutral loopback scenario through the
//! supported direct Rust facade and through `nmp-ffi`, then compare the
//! semantic observations. Each run gets an isolated instance of the SAME
//! `nmp-bdd::relays::ScriptedRelay`; no second relay fake lives here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nmp::{
    AcquisitionEvidence, AuthPhase, Binding, DiagnosticsSnapshot, Durability, Engine, EngineConfig,
    Filter, Lane, LiveQuery, ObservationCancel, ReceiptId, ReceiptReattachment, Row, RowDelta,
    ShortfallFact, SourceStatus, Timestamp, UnsignedEvent, WriteIntent, WritePayload, WriteRouting,
    WriteStatus,
};
use nmp_bdd::relays::{RelayConfig, ScriptedRelay};
use nmp_ffi::convert::{write_status_to_ffi, WriteStatusRef};
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpQueryHandle};
use nmp_ffi::nip02::{
    FfiFollowActionStatus, FfiFollowAvailability, FfiFollowRelationship, FfiFollowSnapshot,
    FollowActionObserver, FollowObserver,
};
use nmp_ffi::observer::{DiagnosticsObserver, ReceiptObserver, RowObserver};
use nmp_ffi::types::{
    FfiAcquisitionEvidence, FfiAuthPhase, FfiBinding, FfiDiagnosticsSnapshot, FfiDurability,
    FfiFilter, FfiFrame, FfiReceiptReattachment, FfiRowDelta, FfiShortfallFact, FfiSourceStatus,
    FfiWriteIntent, FfiWritePayload, FfiWriteRouting, FfiWriteStatus,
};
use nmp_nip02::{
    observe_following, set_following, FollowAction, FollowActionStatus, FollowAvailability,
    FollowChange, FollowObservation, FollowRelationship, FollowSnapshot,
};
use nostr::{JsonUtil, Keys, Kind};

const WAIT: Duration = Duration::from_secs(10);
const DISCOVERY_TRIGGER_KIND: u16 = 9_997;
const QUERY_KIND: u16 = 9_998;
const WRITE_KIND: u16 = 9_999;
const REATTACH_LIVE_KIND: u16 = 9_996;
const REATTACH_TERMINAL_KIND: u16 = 9_995;
const QUERY_CREATED_AT: u64 = 1_700_000_100;
const WRITE_CREATED_AT: u64 = 1_700_000_200;
const SECRET_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

#[test]
fn retry_lane_receipt_truth_projects_exactly_from_direct_rust_to_ffi() {
    let relay = nostr::RelayUrl::parse("wss://receipt-parity.example").unwrap();
    let cases = [
        (
            WriteStatus::AwaitingRelay {
                relay: relay.clone(),
            },
            FfiWriteStatus::AwaitingRelay {
                relay: relay.to_string(),
            },
        ),
        (
            WriteStatus::AwaitingAuth {
                relay: relay.clone(),
            },
            FfiWriteStatus::AwaitingAuth {
                relay: relay.to_string(),
            },
        ),
        (
            WriteStatus::RetryEligible {
                relay: relay.clone(),
                attempt: 7,
                eligible_at: Timestamp::from(123),
            },
            FfiWriteStatus::RetryEligible {
                relay: relay.to_string(),
                attempt: 7,
                eligible_at: 123,
            },
        ),
        (
            WriteStatus::HandoffAmbiguous {
                relay: relay.clone(),
                attempt: 8,
                observed_at: Timestamp::from(124),
            },
            FfiWriteStatus::HandoffAmbiguous {
                relay: relay.to_string(),
                attempt: 8,
                observed_at: 124,
            },
        ),
        (
            WriteStatus::Sent {
                relay: relay.clone(),
                attempt: 9,
                written_at: Timestamp::from(125),
            },
            FfiWriteStatus::Sent {
                relay: relay.to_string(),
                attempt: 9,
                written_at: 125,
            },
        ),
    ];

    for (direct, expected_ffi) in cases {
        assert_eq!(
            write_status_to_ffi(WriteStatusRef(&direct)),
            expected_ffi,
            "direct/FFI parity must retain every relay, ordinal, and timestamp"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormRow {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
    /// #105: the row's relay-provenance set, normalized the same way every
    /// other relay identifier in this file is (loopback placeholder).
    sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormSource {
    relay: String,
    reconciled_through: Option<u64>,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormEvidence {
    sources: Vec<NormSource>,
    shortfall: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormStatus {
    Accepted,
    /// #47 Unit B: carries the parked pubkey (hex) so the direct/FFI
    /// parity proof covers the payload, not just the variant tag.
    AwaitingCapability(String),
    Signed(String),
    Routed(Vec<String>),
    AwaitingRelay(String),
    AwaitingAuth(String),
    RetryEligible(String, u64, u64),
    HandoffAmbiguous(String, u64, u64),
    Sent(String),
    Acked(String),
    Rejected(String, String),
    GaveUp(String),
    PersistenceBlocked(String),
    RoutePersistenceBlocked(String),
    OutcomeUnknown(String),
    ReplaceableConflict(Option<String>, Option<String>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormRelayDiagnostics {
    relay: String,
    wire_sub_count: usize,
    authors_served: usize,
    by_lane: Vec<(String, usize)>,
    filters: Vec<String>,
    events_by_kind: Vec<(u16, u64)>,
    coverage: Vec<(String, Option<(u64, u64)>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormDiagnostics {
    relays: Vec<NormRelayDiagnostics>,
    uncovered_author_count: usize,
    dropped_merge_rules: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandoffBaseline {
    discovery: u64,
    content: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ScenarioOutcome {
    rows: Vec<NormRow>,
    evidence: NormEvidence,
    receipts: Vec<NormStatus>,
    diagnostics: NormDiagnostics,
}

#[derive(Debug, PartialEq, Eq)]
struct TamperedOutcome {
    receipts: Vec<NormStatus>,
    relay_contact_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormFollowSnapshot {
    active_pubkey: Option<String>,
    target: String,
    relationship: &'static str,
    availability: &'static str,
    has_base: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormFollowActionStatus {
    Acquiring,
    NoChange(bool),
    Receipt(&'static str),
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
struct FollowScenarioOutcome {
    initial: NormFollowSnapshot,
    follow: Vec<NormFollowActionStatus>,
    after_follow: NormFollowSnapshot,
    no_change: Vec<NormFollowActionStatus>,
    unfollow: Vec<NormFollowActionStatus>,
    after_unfollow: NormFollowSnapshot,
    preserved_existing_follow: NormFollowSnapshot,
}

struct FfiFollowSnapshots {
    tx: Mutex<mpsc::Sender<FfiFollowSnapshot>>,
}

impl FollowObserver for FfiFollowSnapshots {
    fn on_snapshot(&self, snapshot: FfiFollowSnapshot) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send(snapshot);
    }

    fn on_closed(&self) {}
}

struct FfiFollowActions {
    tx: Mutex<mpsc::Sender<FfiFollowActionStatus>>,
}

impl FollowActionObserver for FfiFollowActions {
    fn on_status(&self, status: FfiFollowActionStatus) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send(status);
    }

    fn on_closed(&self) {}
}

fn fixed_keys() -> Keys {
    Keys::parse(SECRET_KEY).expect("fixed parity key must parse")
}

fn normalize_url(value: &str, relay: &str) -> String {
    if value == relay {
        "<loopback-relay>".to_string()
    } else {
        value.to_string()
    }
}

fn recv_before<T>(rx: &mpsc::Receiver<T>, deadline: Instant, what: &str) -> T {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
        !remaining.is_zero(),
        "{what} did not settle within the total {:?} bound",
        WAIT
    );
    rx.recv_timeout(remaining).unwrap_or_else(|error| {
        panic!("{what} did not settle within the total {WAIT:?} bound: {error}")
    })
}

fn lane_name(lane: Lane) -> &'static str {
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
        Lane::ExplicitPinned => "explicit_pinned",
    }
}

fn direct_status_name(status: SourceStatus) -> String {
    match status {
        SourceStatus::Requesting => "requesting".to_string(),
        SourceStatus::Connecting => "connecting".to_string(),
        SourceStatus::Disconnected => "disconnected".to_string(),
        SourceStatus::AwaitingAuth { phase } => match phase {
            AuthPhase::AwaitingPolicy => "awaiting_auth:policy".to_string(),
            AuthPhase::AwaitingChallenge => "awaiting_auth:challenge".to_string(),
            AuthPhase::AwaitingSignature => "awaiting_auth:signature".to_string(),
            AuthPhase::AwaitingRelayAck => "awaiting_auth:relay_ack".to_string(),
        },
        SourceStatus::AuthDenied => "auth_denied".to_string(),
        SourceStatus::Error => "error".to_string(),
    }
}

fn ffi_status_name(status: FfiSourceStatus) -> String {
    match status {
        FfiSourceStatus::Requesting => "requesting".to_string(),
        FfiSourceStatus::Connecting => "connecting".to_string(),
        FfiSourceStatus::Disconnected => "disconnected".to_string(),
        FfiSourceStatus::AwaitingAuth { phase } => match phase {
            FfiAuthPhase::AwaitingPolicy => "awaiting_auth:policy".to_string(),
            FfiAuthPhase::AwaitingChallenge => "awaiting_auth:challenge".to_string(),
            FfiAuthPhase::AwaitingSignature => "awaiting_auth:signature".to_string(),
            FfiAuthPhase::AwaitingRelayAck => "awaiting_auth:relay_ack".to_string(),
            FfiAuthPhase::Ready => "awaiting_auth:invalid_ready".to_string(),
            FfiAuthPhase::Denied => "awaiting_auth:invalid_denied".to_string(),
            FfiAuthPhase::Error => "awaiting_auth:invalid_error".to_string(),
        },
        FfiSourceStatus::AuthDenied => "auth_denied".to_string(),
        FfiSourceStatus::Error => "error".to_string(),
    }
}

fn normalize_direct_evidence(evidence: AcquisitionEvidence, relay: &str) -> NormEvidence {
    let mut sources = evidence
        .sources
        .into_iter()
        .map(|source| NormSource {
            relay: normalize_url(source.relay.as_str(), relay),
            reconciled_through: source.reconciled_through.map(|time| time.as_secs()),
            status: direct_status_name(source.status),
        })
        .collect::<Vec<_>>();
    sources.sort();
    let mut shortfall = evidence
        .shortfall
        .into_iter()
        .map(|fact| match fact {
            ShortfallFact::NoPlannedSource { atom } => {
                format!("no_planned_source:{}", atom.to_nostr().as_json())
            }
            ShortfallFact::NoResolvedDemand => "no_resolved_demand".to_string(),
            ShortfallFact::LocalLimit { atom } => {
                format!("local_limit:{}", atom.to_nostr().as_json())
            }
        })
        .collect::<Vec<_>>();
    shortfall.sort();
    NormEvidence { sources, shortfall }
}

fn normalize_ffi_evidence(evidence: FfiAcquisitionEvidence, relay: &str) -> NormEvidence {
    let mut sources = evidence
        .sources
        .into_iter()
        .map(|source| NormSource {
            relay: normalize_url(&source.relay, relay),
            reconciled_through: source.reconciled_through,
            status: ffi_status_name(source.status),
        })
        .collect::<Vec<_>>();
    sources.sort();
    let mut shortfall = evidence
        .shortfall
        .into_iter()
        .map(|fact| match fact {
            FfiShortfallFact::NoPlannedSource { atom } => {
                format!("no_planned_source:{atom}")
            }
            FfiShortfallFact::NoResolvedDemand => "no_resolved_demand".to_string(),
            FfiShortfallFact::LocalLimit { atom } => format!("local_limit:{atom}"),
        })
        .collect::<Vec<_>>();
    shortfall.sort();
    NormEvidence { sources, shortfall }
}

fn normalize_direct_status(status: WriteStatus, relay: &str) -> NormStatus {
    match status {
        WriteStatus::Accepted => NormStatus::Accepted,
        WriteStatus::AwaitingCapability { pubkey } => {
            NormStatus::AwaitingCapability(pubkey.to_hex())
        }
        WriteStatus::Signed(id) => NormStatus::Signed(id.to_hex()),
        WriteStatus::Routed(relays) => NormStatus::Routed(
            relays
                .iter()
                .map(|url| normalize_url(url.as_str(), relay))
                .collect(),
        ),
        WriteStatus::AwaitingRelay { relay: url } => {
            NormStatus::AwaitingRelay(normalize_url(url.as_str(), relay))
        }
        WriteStatus::AwaitingAuth { relay: url } => {
            NormStatus::AwaitingAuth(normalize_url(url.as_str(), relay))
        }
        WriteStatus::RetryEligible {
            relay: url,
            attempt,
            eligible_at,
        } => NormStatus::RetryEligible(
            normalize_url(url.as_str(), relay),
            attempt,
            eligible_at.as_secs(),
        ),
        WriteStatus::HandoffAmbiguous {
            relay: url,
            attempt,
            observed_at,
        } => NormStatus::HandoffAmbiguous(
            normalize_url(url.as_str(), relay),
            attempt,
            observed_at.as_secs(),
        ),
        WriteStatus::Sent { relay: url, .. } => {
            NormStatus::Sent(normalize_url(url.as_str(), relay))
        }
        WriteStatus::Acked(url) => NormStatus::Acked(normalize_url(url.as_str(), relay)),
        WriteStatus::Rejected(url, reason) => {
            NormStatus::Rejected(normalize_url(url.as_str(), relay), reason)
        }
        WriteStatus::GaveUp(url) => NormStatus::GaveUp(normalize_url(url.as_str(), relay)),
        WriteStatus::PersistenceBlocked(url) => {
            NormStatus::PersistenceBlocked(normalize_url(url.as_str(), relay))
        }
        WriteStatus::RoutePersistenceBlocked(url) => {
            NormStatus::RoutePersistenceBlocked(normalize_url(url.as_str(), relay))
        }
        WriteStatus::OutcomeUnknown(url) => {
            NormStatus::OutcomeUnknown(normalize_url(url.as_str(), relay))
        }
        WriteStatus::ReplaceableConflict { expected, actual } => NormStatus::ReplaceableConflict(
            expected.map(|id| id.to_hex()),
            actual.map(|id| id.to_hex()),
        ),
        WriteStatus::Failed(reason) => NormStatus::Failed(reason),
    }
}

fn normalize_ffi_status(status: FfiWriteStatus, relay: &str) -> NormStatus {
    match status {
        FfiWriteStatus::Accepted => NormStatus::Accepted,
        FfiWriteStatus::AwaitingCapability { pubkey } => NormStatus::AwaitingCapability(pubkey),
        FfiWriteStatus::Signed { event_id } => NormStatus::Signed(event_id),
        FfiWriteStatus::Routed { mut relays } => {
            for url in &mut relays {
                *url = normalize_url(url, relay);
            }
            relays.sort();
            NormStatus::Routed(relays)
        }
        FfiWriteStatus::AwaitingRelay { relay: url } => {
            NormStatus::AwaitingRelay(normalize_url(&url, relay))
        }
        FfiWriteStatus::AwaitingAuth { relay: url } => {
            NormStatus::AwaitingAuth(normalize_url(&url, relay))
        }
        FfiWriteStatus::RetryEligible {
            relay: url,
            attempt,
            eligible_at,
        } => NormStatus::RetryEligible(normalize_url(&url, relay), attempt, eligible_at),
        FfiWriteStatus::HandoffAmbiguous {
            relay: url,
            attempt,
            observed_at,
        } => NormStatus::HandoffAmbiguous(normalize_url(&url, relay), attempt, observed_at),
        FfiWriteStatus::Sent { relay: url, .. } => NormStatus::Sent(normalize_url(&url, relay)),
        FfiWriteStatus::Acked { relay: url } => NormStatus::Acked(normalize_url(&url, relay)),
        FfiWriteStatus::Rejected { relay: url, reason } => {
            NormStatus::Rejected(normalize_url(&url, relay), reason)
        }
        FfiWriteStatus::GaveUp { relay: url } => NormStatus::GaveUp(normalize_url(&url, relay)),
        FfiWriteStatus::PersistenceBlocked { relay: url } => {
            NormStatus::PersistenceBlocked(normalize_url(&url, relay))
        }
        FfiWriteStatus::RoutePersistenceBlocked { relay: url } => {
            NormStatus::RoutePersistenceBlocked(normalize_url(&url, relay))
        }
        FfiWriteStatus::OutcomeUnknown { relay: url } => {
            NormStatus::OutcomeUnknown(normalize_url(&url, relay))
        }
        FfiWriteStatus::ReplaceableConflict { expected, actual } => {
            NormStatus::ReplaceableConflict(expected, actual)
        }
        FfiWriteStatus::Failed { reason } => NormStatus::Failed(reason),
    }
}

fn normalize_direct_diagnostics(snapshot: DiagnosticsSnapshot, relay: &str) -> NormDiagnostics {
    let mut relays = snapshot
        .relays
        .into_iter()
        .map(|entry| {
            let mut by_lane = entry
                .by_lane
                .into_iter()
                .map(|(lane, count)| (lane_name(lane).to_string(), count))
                .collect::<Vec<_>>();
            by_lane.sort();
            let mut filters = entry.filters;
            filters.sort();
            let mut events_by_kind = entry.events_by_kind;
            events_by_kind.sort();
            let mut coverage = entry
                .coverage
                .into_iter()
                .map(|coverage| {
                    (
                        coverage.filter,
                        coverage
                            .coverage
                            .map(|interval| (interval.from.as_secs(), interval.through.as_secs())),
                    )
                })
                .collect::<Vec<_>>();
            coverage.sort();
            NormRelayDiagnostics {
                relay: normalize_url(entry.relay.as_str(), relay),
                wire_sub_count: entry.wire_sub_count,
                authors_served: entry.authors_served,
                by_lane,
                filters,
                events_by_kind,
                coverage,
            }
        })
        .collect::<Vec<_>>();
    relays.sort();
    let mut dropped_merge_rules = snapshot
        .dropped_merge_rules
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    dropped_merge_rules.sort();
    NormDiagnostics {
        relays,
        uncovered_author_count: snapshot.uncovered_author_count,
        dropped_merge_rules,
    }
}

fn normalize_ffi_diagnostics(snapshot: FfiDiagnosticsSnapshot, relay: &str) -> NormDiagnostics {
    let mut relays = snapshot
        .relays
        .into_iter()
        .map(|entry| {
            let mut by_lane = entry
                .by_lane
                .into_iter()
                .map(|lane| (lane.lane, lane.count as usize))
                .collect::<Vec<_>>();
            by_lane.sort();
            let mut filters = entry.filters;
            filters.sort();
            let mut events_by_kind = entry
                .events_by_kind
                .into_iter()
                .map(|kind| (kind.kind, kind.count))
                .collect::<Vec<_>>();
            events_by_kind.sort();
            let mut coverage = entry
                .coverage
                .into_iter()
                .map(|coverage| {
                    (
                        coverage.filter,
                        coverage
                            .coverage
                            .map(|interval| (interval.from, interval.through)),
                    )
                })
                .collect::<Vec<_>>();
            coverage.sort();
            NormRelayDiagnostics {
                relay: normalize_url(&entry.relay, relay),
                wire_sub_count: entry.wire_sub_count as usize,
                authors_served: entry.authors_served as usize,
                by_lane,
                filters,
                events_by_kind,
                coverage,
            }
        })
        .collect::<Vec<_>>();
    relays.sort();
    let mut dropped_merge_rules = snapshot.dropped_merge_rules;
    dropped_merge_rules.sort();
    NormDiagnostics {
        relays,
        uncovered_author_count: snapshot.uncovered_author_count as usize,
        dropped_merge_rules,
    }
}

fn direct_filter(pubkey: &str, kind: u16) -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([kind])),
        authors: Some(Binding::Literal(BTreeSet::from([pubkey.to_string()]))),
        limit: Some(10),
        ..Filter::default()
    }
}

fn ffi_filter(pubkey: &str, kind: u16) -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![kind]),
        authors: Some(FfiBinding::Literal {
            values: vec![pubkey.to_string()],
        }),
        limit: Some(10),
        ..FfiFilter::default()
    }
}

fn direct_row(row: &Row, relay: &str) -> NormRow {
    let event = &row.event;
    NormRow {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16(),
        tags: event.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
        sources: row
            .sources
            .iter()
            .map(|url| normalize_url(url.as_str(), relay))
            .collect(),
    }
}

fn apply_direct_deltas(rows: &mut BTreeMap<String, NormRow>, deltas: Vec<RowDelta>, relay: &str) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) => {
                let normalized = direct_row(&row, relay);
                rows.insert(normalized.id.clone(), normalized);
            }
            RowDelta::SourcesGrew { id, sources } => {
                let id = id.to_hex();
                if let Some(existing) = rows.get_mut(&id) {
                    existing.sources = sources
                        .iter()
                        .map(|url| normalize_url(url.as_str(), relay))
                        .collect();
                }
            }
            RowDelta::Removed(id) => {
                rows.remove(&id.to_hex());
            }
        }
    }
}

fn apply_ffi_deltas(rows: &mut BTreeMap<String, NormRow>, deltas: Vec<FfiRowDelta>, relay: &str) {
    for delta in deltas {
        match delta {
            FfiRowDelta::Added { row } => {
                let normalized = NormRow {
                    id: row.id,
                    pubkey: row.pubkey,
                    created_at: row.created_at,
                    kind: row.kind,
                    tags: row.tags,
                    content: row.content,
                    sig: row.sig,
                    sources: row
                        .sources
                        .iter()
                        .map(|url| normalize_url(url, relay))
                        .collect(),
                };
                rows.insert(normalized.id.clone(), normalized);
            }
            FfiRowDelta::SourcesGrew { id, sources } => {
                if let Some(existing) = rows.get_mut(&id) {
                    existing.sources = sources
                        .iter()
                        .map(|url| normalize_url(url, relay))
                        .collect();
                }
            }
            FfiRowDelta::Removed { id } => {
                rows.remove(&id);
            }
        }
    }
}

fn filter_names_kind(filter: &str, kind: u16) -> bool {
    filter.contains(&format!("\"kinds\":[{kind}]"))
}

fn event_count(relay: &NormRelayDiagnostics, kind: u16) -> u64 {
    relay
        .events_by_kind
        .iter()
        .find_map(|(got, count)| (*got == kind).then_some(*count))
        .unwrap_or(0)
}

fn handoff_is_quiescent(
    snapshot: &NormDiagnostics,
    relay_witness: &ScriptedRelay,
) -> Option<HandoffBaseline> {
    let [relay] = snapshot.relays.as_slice() else {
        return None;
    };
    let has_discovery = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, DISCOVERY_TRIGGER_KIND));
    let has_content = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, QUERY_KIND));
    let has_internal_discovery = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, Kind::RelayList.as_u16()));
    let routed_through_nip65 = relay
        .by_lane
        .iter()
        .any(|(lane, count)| lane == "nip65_write" && *count > 0);
    let baseline = HandoffBaseline {
        discovery: relay_witness.query_count_for_kind(Kind::RelayList.as_u16()),
        content: relay_witness.query_count_for_kind(QUERY_KIND),
    };
    (has_discovery
        && has_content
        && !has_internal_discovery
        && routed_through_nip65
        && baseline.discovery != 0
        && baseline.content != 0
        && event_count(relay, Kind::RelayList.as_u16()) == baseline.discovery
        && event_count(relay, QUERY_KIND) == baseline.content)
        .then_some(baseline)
}

fn content_phase_is_quiescent(
    snapshot: &NormDiagnostics,
    baseline: HandoffBaseline,
    relay_witness: &ScriptedRelay,
) -> bool {
    let [relay] = snapshot.relays.as_slice() else {
        return false;
    };
    let has_content = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, QUERY_KIND));
    let has_stale_filter = relay.filters.iter().any(|filter| {
        filter_names_kind(filter, DISCOVERY_TRIGGER_KIND)
            || filter_names_kind(filter, Kind::RelayList.as_u16())
    });
    let routed_through_nip65 = relay
        .by_lane
        .iter()
        .any(|(lane, count)| lane == "nip65_write" && *count > 0);
    let content_req_count = relay_witness.query_count_for_kind(QUERY_KIND);
    let discovery_req_count = relay_witness.query_count_for_kind(Kind::RelayList.as_u16());
    has_content
        && !has_stale_filter
        && routed_through_nip65
        && content_req_count == baseline.content
        && discovery_req_count == baseline.discovery
        && event_count(relay, QUERY_KIND) == baseline.content
        && event_count(relay, Kind::RelayList.as_u16()) == baseline.discovery
        && !relay.coverage.is_empty()
        && relay
            .coverage
            .iter()
            .all(|(_, coverage)| coverage.is_none())
}

fn assert_content_phase_diagnostics(
    snapshot: &NormDiagnostics,
    baseline: HandoffBaseline,
    relay: &ScriptedRelay,
    surface: &str,
) {
    assert!(
        content_phase_is_quiescent(snapshot, baseline, relay),
        "{surface} diagnostics must contain only the discovered NIP-65-routed content plan, \
         with content/discovery REQs and events unchanged from the drained handoff \
         baseline {baseline:?}: {snapshot:?}"
    );
}

fn wait_for_direct_handoff_quiescence(
    rx: &mpsc::Receiver<DiagnosticsSnapshot>,
    relay: &ScriptedRelay,
) -> HandoffBaseline {
    let deadline = Instant::now() + WAIT;
    let mut last_diagnostics = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = rx.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "direct handoff diagnostics did not settle within the total {WAIT:?} bound: \
                 {error}; last snapshot: {last_diagnostics:?}; relay query counts: \
                 discovery={}, content={}",
                relay.query_count_for_kind(Kind::RelayList.as_u16()),
                relay.query_count_for_kind(QUERY_KIND),
            )
        });
        let snapshot = normalize_direct_diagnostics(snapshot, relay.url.as_str());
        if let Some(baseline) = handoff_is_quiescent(&snapshot, relay) {
            return baseline;
        }
        last_diagnostics = Some(snapshot);
    }
}

fn wait_for_ffi_handoff_quiescence(
    rx: &mpsc::Receiver<FfiDiagnosticsSnapshot>,
    relay: &ScriptedRelay,
) -> HandoffBaseline {
    let deadline = Instant::now() + WAIT;
    let mut last_diagnostics = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = rx.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "FFI handoff diagnostics did not settle within the total {WAIT:?} bound: \
                 {error}; last snapshot: {last_diagnostics:?}; relay query counts: \
                 discovery={}, content={}",
                relay.query_count_for_kind(Kind::RelayList.as_u16()),
                relay.query_count_for_kind(QUERY_KIND),
            )
        });
        let snapshot = normalize_ffi_diagnostics(snapshot, relay.url.as_str());
        if let Some(baseline) = handoff_is_quiescent(&snapshot, relay) {
            return baseline;
        }
        last_diagnostics = Some(snapshot);
    }
}

fn expected_limited_evidence() -> NormEvidence {
    NormEvidence {
        sources: vec![NormSource {
            relay: "<loopback-relay>".to_string(),
            reconciled_through: None,
            status: "requesting".to_string(),
        }],
        shortfall: vec![],
    }
}

fn collect_direct_receipts(rx: mpsc::Receiver<WriteStatus>, relay: &str) -> Vec<NormStatus> {
    let mut statuses = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let status = recv_before(&rx, deadline, "direct receipt");
        let normalized = normalize_direct_status(status, relay);
        let terminal = matches!(
            normalized,
            NormStatus::Acked(_)
                | NormStatus::Rejected(_, _)
                | NormStatus::GaveUp(_)
                | NormStatus::Failed(_)
        );
        statuses.push(normalized);
        if terminal {
            return statuses;
        }
    }
}

/// Bounded sibling of [`collect_direct_receipts`] for the fail-closed AUTH
/// park: there IS no terminal status (the lane parks), so collection stops
/// at the first `AwaitingAuth` beat instead. Borrows the receiver so the
/// caller can afterwards prove NO further status arrives.
fn collect_direct_receipts_until_awaiting_auth(
    rx: &mpsc::Receiver<WriteStatus>,
    relay: &str,
) -> Vec<NormStatus> {
    let mut statuses = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let status = recv_before(rx, deadline, "direct auth-parked receipt");
        let normalized = normalize_direct_status(status, relay);
        // #8 U4: the first `AwaitingAuth` beat is the bounded AUTH-discovery
        // park on the cold protected session (before `Sent`); the park under
        // test is the one the relay's `auth-required:` refusal causes AFTER
        // the send.
        let sent = statuses
            .iter()
            .any(|status| matches!(status, NormStatus::Sent(_)));
        let parked = sent && matches!(normalized, NormStatus::AwaitingAuth(_));
        statuses.push(normalized);
        if parked {
            return statuses;
        }
    }
}

fn collect_ffi_receipts_until_awaiting_auth(
    rx: &mpsc::Receiver<FfiWriteStatus>,
    relay: &str,
) -> Vec<NormStatus> {
    let mut statuses = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let status = recv_before(rx, deadline, "FFI auth-parked receipt");
        let normalized = normalize_ffi_status(status, relay);
        let sent = statuses
            .iter()
            .any(|status| matches!(status, NormStatus::Sent(_)));
        let parked = sent && matches!(normalized, NormStatus::AwaitingAuth(_));
        statuses.push(normalized);
        if parked {
            return statuses;
        }
    }
}

/// The exact ordered pre-ack facts every durable parity write now exposes.
/// #8 U2: durable writes ride the cold `AccessContext::Nip42` session
/// instead of the already-warm public read session, so the reducer emits
/// one deterministic `AwaitingRelay` beat between `Routed` and `Sent` (it
/// schedules the eligible lane in the same turn that dials the session,
/// before that dial can possibly complete) — for EVERY durable write, since
/// worker reconciliation closes the write session once a write terminates.
/// #8 U4 adds the second deterministic beat: once the cold protected
/// session connects, its bounded initial AUTH-discovery window parks the
/// lane as `AwaitingAuth` until the transport's ordered first-read
/// completion releases it (a relay that never challenges releases within
/// the window; one that does parks it for real).
fn expected_send_preamble(keys: &Keys) -> Vec<NormStatus> {
    let event = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    )
    .sign_with_keys(keys)
    .expect("expected receipt fixture must sign cleanly");
    let relay = "<loopback-relay>".to_string();
    vec![
        NormStatus::Accepted,
        NormStatus::Signed(event.id.to_hex()),
        NormStatus::Routed(vec![relay.clone()]),
        NormStatus::AwaitingRelay(relay.clone()),
        NormStatus::AwaitingAuth(relay.clone()),
        NormStatus::Sent(relay),
    ]
}

fn expected_success_receipts(keys: &Keys) -> Vec<NormStatus> {
    let mut receipts = expected_send_preamble(keys);
    receipts.push(NormStatus::Acked("<loopback-relay>".to_string()));
    receipts
}

/// #8 U2 fail-closed park: no AUTH policy registry exists at this wave, so
/// against a relay that answers an unauthenticated EVENT with
/// `OK false "auth-required:"` the write emits exactly one `AwaitingAuth`
/// beat and the lane stays parked — no retry, no terminal status.
fn expected_auth_parked_receipts(keys: &Keys) -> Vec<NormStatus> {
    let mut receipts = expected_send_preamble(keys);
    receipts.push(NormStatus::AwaitingAuth("<loopback-relay>".to_string()));
    receipts
}

struct FfiRows {
    tx: Mutex<mpsc::Sender<(Vec<FfiRowDelta>, FfiAcquisitionEvidence)>>,
}

impl RowObserver for FfiRows {
    fn on_frame(&self, frame: FfiFrame) {
        // Unbounded FFI observations carry deltas + evidence; `window` is
        // always `None` (windowing is a policy on the read noun, #485).
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send((frame.deltas, frame.evidence));
    }

    fn on_closed(&self) {}
}

struct FfiDiagnostics {
    tx: Mutex<mpsc::Sender<FfiDiagnosticsSnapshot>>,
}

impl DiagnosticsObserver for FfiDiagnostics {
    fn on_snapshot(&self, snapshot: FfiDiagnosticsSnapshot) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send(snapshot);
    }

    fn on_closed(&self) {}
}

struct FfiReceipts {
    tx: Mutex<mpsc::Sender<FfiWriteStatus>>,
}

fn stage_direct_discovery(
    engine: &Engine,
    pubkey: &str,
    relay: &ScriptedRelay,
) -> ObservationCancel {
    let subscription = engine
        .observe(
            LiveQuery::from_filter(direct_filter(pubkey, DISCOVERY_TRIGGER_KIND)),
            None,
        )
        .expect("direct discovery query must open");
    let cancel = subscription.cancel_handle();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(batch) = subscription.recv() {
            if tx.send(batch).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + WAIT;
    loop {
        let frame = recv_before(&rx, deadline, "direct discovery query");
        let evidence = normalize_direct_evidence(frame.evidence, relay.url.as_str());
        if evidence == expected_limited_evidence() {
            break;
        }
    }
    cancel
}

fn stage_ffi_discovery(
    engine: &NmpEngine,
    pubkey: &str,
    relay: &ScriptedRelay,
) -> Arc<NmpQueryHandle> {
    let (tx, rx) = mpsc::channel();
    let handle = engine
        .observe(
            ffi_filter(pubkey, DISCOVERY_TRIGGER_KIND),
            None,
            Box::new(FfiRows { tx: Mutex::new(tx) }),
        )
        .expect("FFI discovery query must open");

    let deadline = Instant::now() + WAIT;
    loop {
        let (_deltas, evidence) = recv_before(&rx, deadline, "FFI discovery query");
        let evidence = normalize_ffi_evidence(evidence, relay.url.as_str());
        if evidence == expected_limited_evidence() {
            break;
        }
    }
    handle
}

impl ReceiptObserver for FfiReceipts {
    fn on_status(&self, status: FfiWriteStatus) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send(status);
    }

    fn on_closed(&self) {}
}

async fn setup_relay(keys: &Keys, query_event: &nostr::Event) -> ScriptedRelay {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay.seed_own_relay_list(keys, QUERY_CREATED_AT - 1).await;
    relay.seed_signed_event(query_event).await;
    relay
}

fn normalize_direct_follow_snapshot(snapshot: FollowSnapshot) -> NormFollowSnapshot {
    NormFollowSnapshot {
        active_pubkey: snapshot.active_pubkey.map(|pubkey| pubkey.to_hex()),
        target: snapshot.target.to_hex(),
        relationship: match snapshot.relationship {
            FollowRelationship::Unknown => "unknown",
            FollowRelationship::NotFollowing => "not_following",
            FollowRelationship::Following => "following",
        },
        availability: match snapshot.availability {
            FollowAvailability::SignedOut => "signed_out",
            FollowAvailability::Acquiring => "acquiring",
            FollowAvailability::Ready => "ready",
            FollowAvailability::NoContactList => "no_contact_list",
            FollowAvailability::CachedOnly => "cached_only",
            FollowAvailability::SourceUnavailable => "source_unavailable",
        },
        has_base: snapshot.base_event_id.is_some(),
    }
}

fn normalize_ffi_follow_snapshot(snapshot: FfiFollowSnapshot) -> NormFollowSnapshot {
    NormFollowSnapshot {
        active_pubkey: snapshot.active_pubkey,
        target: snapshot.target,
        relationship: match snapshot.relationship {
            FfiFollowRelationship::Unknown => "unknown",
            FfiFollowRelationship::NotFollowing => "not_following",
            FfiFollowRelationship::Following => "following",
        },
        availability: match snapshot.availability {
            FfiFollowAvailability::SignedOut => "signed_out",
            FfiFollowAvailability::Acquiring => "acquiring",
            FfiFollowAvailability::Ready => "ready",
            FfiFollowAvailability::NoContactList => "no_contact_list",
            FfiFollowAvailability::CachedOnly => "cached_only",
            FfiFollowAvailability::SourceUnavailable => "source_unavailable",
        },
        has_base: snapshot.base_event_id.is_some(),
    }
}

fn direct_follow_receipt_name(status: &WriteStatus) -> &'static str {
    match status {
        WriteStatus::Accepted => "accepted",
        WriteStatus::AwaitingCapability { .. } => "awaiting_capability",
        WriteStatus::Signed(_) => "signed",
        WriteStatus::Routed(_) => "routed",
        WriteStatus::AwaitingRelay { .. } => "awaiting_relay",
        WriteStatus::AwaitingAuth { .. } => "awaiting_auth",
        WriteStatus::RetryEligible { .. } => "retry_eligible",
        WriteStatus::HandoffAmbiguous { .. } => "handoff_ambiguous",
        WriteStatus::Sent { .. } => "sent",
        WriteStatus::Acked(_) => "acked",
        WriteStatus::Rejected(_, _) => "rejected",
        WriteStatus::GaveUp(_) => "gave_up",
        WriteStatus::PersistenceBlocked(_) => "persistence_blocked",
        WriteStatus::RoutePersistenceBlocked(_) => "route_persistence_blocked",
        WriteStatus::OutcomeUnknown(_) => "outcome_unknown",
        WriteStatus::ReplaceableConflict { .. } => "replaceable_conflict",
        WriteStatus::Failed(_) => "failed",
    }
}

fn ffi_follow_receipt_name(status: &FfiWriteStatus) -> &'static str {
    match status {
        FfiWriteStatus::Accepted => "accepted",
        FfiWriteStatus::AwaitingCapability { .. } => "awaiting_capability",
        FfiWriteStatus::Signed { .. } => "signed",
        FfiWriteStatus::Routed { .. } => "routed",
        FfiWriteStatus::AwaitingRelay { .. } => "awaiting_relay",
        FfiWriteStatus::AwaitingAuth { .. } => "awaiting_auth",
        FfiWriteStatus::RetryEligible { .. } => "retry_eligible",
        FfiWriteStatus::HandoffAmbiguous { .. } => "handoff_ambiguous",
        FfiWriteStatus::Sent { .. } => "sent",
        FfiWriteStatus::Acked { .. } => "acked",
        FfiWriteStatus::Rejected { .. } => "rejected",
        FfiWriteStatus::GaveUp { .. } => "gave_up",
        FfiWriteStatus::PersistenceBlocked { .. } => "persistence_blocked",
        FfiWriteStatus::RoutePersistenceBlocked { .. } => "route_persistence_blocked",
        FfiWriteStatus::OutcomeUnknown { .. } => "outcome_unknown",
        FfiWriteStatus::ReplaceableConflict { .. } => "replaceable_conflict",
        FfiWriteStatus::Failed { .. } => "failed",
    }
}

fn collect_direct_follow_action(action: FollowAction) -> Vec<NormFollowActionStatus> {
    let deadline = Instant::now() + WAIT;
    let mut result = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let status = action
            .recv_timeout(remaining)
            .expect("direct follow action must settle before the total deadline");
        let normalized = match status {
            FollowActionStatus::Acquiring => NormFollowActionStatus::Acquiring,
            FollowActionStatus::NoChange { following } => {
                NormFollowActionStatus::NoChange(following)
            }
            FollowActionStatus::Receipt { status, .. } => {
                NormFollowActionStatus::Receipt(direct_follow_receipt_name(&status))
            }
            FollowActionStatus::Failed(failure) => {
                NormFollowActionStatus::Failed(format!("{failure:?}"))
            }
        };
        let terminal = matches!(
            normalized,
            NormFollowActionStatus::NoChange(_)
                | NormFollowActionStatus::Failed(_)
                | NormFollowActionStatus::Receipt(
                    "acked" | "rejected" | "gave_up" | "replaceable_conflict" | "failed"
                )
        );
        result.push(normalized);
        if terminal {
            return result;
        }
    }
}

fn collect_ffi_follow_action(
    rx: &mpsc::Receiver<FfiFollowActionStatus>,
) -> Vec<NormFollowActionStatus> {
    let deadline = Instant::now() + WAIT;
    let mut result = Vec::new();
    loop {
        let status = recv_before(rx, deadline, "FFI follow action");
        let normalized = match status {
            FfiFollowActionStatus::Acquiring => NormFollowActionStatus::Acquiring,
            FfiFollowActionStatus::NoChange { following } => {
                NormFollowActionStatus::NoChange(following)
            }
            FfiFollowActionStatus::Receipt { status, .. } => {
                NormFollowActionStatus::Receipt(ffi_follow_receipt_name(&status))
            }
            FfiFollowActionStatus::Failed { failure } => {
                NormFollowActionStatus::Failed(format!("{failure:?}"))
            }
        };
        let terminal = matches!(
            normalized,
            NormFollowActionStatus::NoChange(_)
                | NormFollowActionStatus::Failed(_)
                | NormFollowActionStatus::Receipt(
                    "acked" | "rejected" | "gave_up" | "replaceable_conflict" | "failed"
                )
        );
        result.push(normalized);
        if terminal {
            return result;
        }
    }
}

fn wait_for_direct_follow_snapshot(
    observation: &FollowObservation,
    relationship: FollowRelationship,
) -> NormFollowSnapshot {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = observation
            .recv_timeout(remaining)
            .expect("direct following observation must settle before the total deadline");
        if snapshot.relationship == relationship
            && snapshot.availability == FollowAvailability::Ready
        {
            return normalize_direct_follow_snapshot(snapshot);
        }
    }
}

fn wait_for_ffi_follow_snapshot(
    rx: &mpsc::Receiver<FfiFollowSnapshot>,
    relationship: FfiFollowRelationship,
) -> NormFollowSnapshot {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = recv_before(rx, deadline, "FFI following observation");
        if snapshot.relationship == relationship
            && snapshot.availability == FfiFollowAvailability::Ready
        {
            return normalize_ffi_follow_snapshot(snapshot);
        }
    }
}

fn wait_for_direct_follow_availability(
    observation: &FollowObservation,
    availability: FollowAvailability,
) -> NormFollowSnapshot {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = observation
            .recv_timeout(remaining)
            .expect("direct following availability must settle before the total deadline");
        if snapshot.availability == availability {
            return normalize_direct_follow_snapshot(snapshot);
        }
    }
}

fn wait_for_ffi_follow_availability(
    rx: &mpsc::Receiver<FfiFollowSnapshot>,
    availability: FfiFollowAvailability,
) -> NormFollowSnapshot {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = recv_before(rx, deadline, "FFI following availability");
        if snapshot.availability == availability {
            return normalize_ffi_follow_snapshot(snapshot);
        }
    }
}

async fn setup_follow_relay(author: &Keys, existing: &Keys) -> ScriptedRelay {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay
        .seed_own_relay_list(author, QUERY_CREATED_AT - 1)
        .await;
    relay
        .seed_contact_list(author, &[existing.public_key()], QUERY_CREATED_AT)
        .await;
    relay
}

async fn run_direct_follow_scenario(
    author: &Keys,
    existing: &Keys,
    target: &Keys,
) -> FollowScenarioOutcome {
    let relay = setup_follow_relay(author, existing).await;
    let engine = Arc::new(
        Engine::new(EngineConfig {
            indexer_relays: vec![relay.url.to_string()],
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..EngineConfig::default()
        })
        .expect("direct follow engine must construct"),
    );
    let active = engine
        .add_account(&author.secret_key().to_secret_hex())
        .expect("direct follow account must register")
        .public_key();
    engine
        .set_active_account(Some(active))
        .expect("direct follow account must activate");

    let observation = observe_following(engine.clone(), target.public_key())
        .expect("direct following observation must open");
    let initial = wait_for_direct_follow_snapshot(&observation, FollowRelationship::NotFollowing);

    let follow = collect_direct_follow_action(set_following(
        engine.clone(),
        target.public_key(),
        FollowChange::Follow,
    ));
    let after_follow = wait_for_direct_follow_snapshot(&observation, FollowRelationship::Following);

    let no_change = collect_direct_follow_action(set_following(
        engine.clone(),
        target.public_key(),
        FollowChange::Follow,
    ));

    let unfollow = collect_direct_follow_action(set_following(
        engine.clone(),
        target.public_key(),
        FollowChange::Unfollow,
    ));
    let after_unfollow =
        wait_for_direct_follow_snapshot(&observation, FollowRelationship::NotFollowing);

    let existing_observation = observe_following(engine.clone(), existing.public_key())
        .expect("direct preserved-follow observation must open");
    let preserved_existing_follow =
        wait_for_direct_follow_snapshot(&existing_observation, FollowRelationship::Following);

    drop(existing_observation);
    drop(observation);
    engine.shutdown();
    relay.shutdown();

    FollowScenarioOutcome {
        initial,
        follow,
        after_follow,
        no_change,
        unfollow,
        after_unfollow,
        preserved_existing_follow,
    }
}

async fn run_ffi_follow_scenario(
    author: &Keys,
    existing: &Keys,
    target: &Keys,
) -> FollowScenarioOutcome {
    let relay = setup_follow_relay(author, existing).await;
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        indexer_relays: vec![relay.url.to_string()],
        app_relays: vec![],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("FFI follow engine must construct");
    let active = engine
        .add_account(author.secret_key().to_secret_hex())
        .expect("FFI follow account must register");
    engine
        .set_active_account(Some(active.public_key()))
        .expect("FFI follow account must activate");

    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let observation = engine
        .observe_following(
            target.public_key().to_hex(),
            Box::new(FfiFollowSnapshots {
                tx: Mutex::new(snapshot_tx),
            }),
        )
        .expect("FFI following observation must open");
    let initial = wait_for_ffi_follow_snapshot(&snapshot_rx, FfiFollowRelationship::NotFollowing);

    let (follow_tx, follow_rx) = mpsc::channel();
    engine.follow(
        target.public_key().to_hex(),
        Box::new(FfiFollowActions {
            tx: Mutex::new(follow_tx),
        }),
    );
    let follow = collect_ffi_follow_action(&follow_rx);
    let after_follow = wait_for_ffi_follow_snapshot(&snapshot_rx, FfiFollowRelationship::Following);

    let (no_change_tx, no_change_rx) = mpsc::channel();
    engine.follow(
        target.public_key().to_hex(),
        Box::new(FfiFollowActions {
            tx: Mutex::new(no_change_tx),
        }),
    );
    let no_change = collect_ffi_follow_action(&no_change_rx);

    let (unfollow_tx, unfollow_rx) = mpsc::channel();
    engine.unfollow(
        target.public_key().to_hex(),
        Box::new(FfiFollowActions {
            tx: Mutex::new(unfollow_tx),
        }),
    );
    let unfollow = collect_ffi_follow_action(&unfollow_rx);
    let after_unfollow =
        wait_for_ffi_follow_snapshot(&snapshot_rx, FfiFollowRelationship::NotFollowing);

    let (existing_tx, existing_rx) = mpsc::channel();
    let existing_observation = engine
        .observe_following(
            existing.public_key().to_hex(),
            Box::new(FfiFollowSnapshots {
                tx: Mutex::new(existing_tx),
            }),
        )
        .expect("FFI preserved-follow observation must open");
    let preserved_existing_follow =
        wait_for_ffi_follow_snapshot(&existing_rx, FfiFollowRelationship::Following);

    existing_observation.cancel();
    observation.cancel();
    engine.shutdown();
    relay.shutdown();

    FollowScenarioOutcome {
        initial,
        follow,
        after_follow,
        no_change,
        unfollow,
        after_unfollow,
        preserved_existing_follow,
    }
}

async fn run_direct_missing_contact_list(
    author: &Keys,
    target: &Keys,
) -> (NormFollowSnapshot, Vec<NormFollowActionStatus>) {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay
        .seed_own_relay_list(author, QUERY_CREATED_AT - 1)
        .await;
    let engine = Arc::new(
        Engine::new(EngineConfig {
            indexer_relays: vec![relay.url.to_string()],
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..EngineConfig::default()
        })
        .expect("direct missing-list engine must construct"),
    );
    let active = engine
        .add_account(&author.secret_key().to_secret_hex())
        .expect("direct missing-list account must register")
        .public_key();
    engine
        .set_active_account(Some(active))
        .expect("direct missing-list account must activate");

    let observation = observe_following(engine.clone(), target.public_key())
        .expect("direct missing-list observation must open");
    let snapshot =
        wait_for_direct_follow_availability(&observation, FollowAvailability::NoContactList);
    let action = collect_direct_follow_action(set_following(
        engine.clone(),
        target.public_key(),
        FollowChange::Follow,
    ));

    drop(observation);
    engine.shutdown();
    relay.shutdown();
    (snapshot, action)
}

async fn run_ffi_missing_contact_list(
    author: &Keys,
    target: &Keys,
) -> (NormFollowSnapshot, Vec<NormFollowActionStatus>) {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay
        .seed_own_relay_list(author, QUERY_CREATED_AT - 1)
        .await;
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        indexer_relays: vec![relay.url.to_string()],
        app_relays: vec![],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("FFI missing-list engine must construct");
    let active = engine
        .add_account(author.secret_key().to_secret_hex())
        .expect("FFI missing-list account must register");
    engine
        .set_active_account(Some(active.public_key()))
        .expect("FFI missing-list account must activate");

    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let observation = engine
        .observe_following(
            target.public_key().to_hex(),
            Box::new(FfiFollowSnapshots {
                tx: Mutex::new(snapshot_tx),
            }),
        )
        .expect("FFI missing-list observation must open");
    let snapshot =
        wait_for_ffi_follow_availability(&snapshot_rx, FfiFollowAvailability::NoContactList);
    let (action_tx, action_rx) = mpsc::channel();
    engine.follow(
        target.public_key().to_hex(),
        Box::new(FfiFollowActions {
            tx: Mutex::new(action_tx),
        }),
    );
    let action = collect_ffi_follow_action(&action_rx);

    observation.cancel();
    engine.shutdown();
    relay.shutdown();
    (snapshot, action)
}

async fn run_direct_success(keys: &Keys, query_event: &nostr::Event) -> ScenarioOutcome {
    let relay = setup_relay(keys, query_event).await;
    let expected_row_id = query_event.id.to_hex();
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![relay_url.clone()],
        // This scenario exercises the REAL discovery path: the author's
        // seeded kind:10002 names this loopback relay as its write relay, so
        // it arrives as a DISCOVERED relay and must be opted past the SSRF
        // admission policy (issue #121) for the test's loopback host.
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..EngineConfig::default()
    })
    .expect("direct engine must construct");
    let pubkey = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("direct account must register")
        .public_key();
    engine
        .set_active_account(Some(pubkey))
        .expect("direct account must activate");

    let diagnostics = engine
        .observe_diagnostics()
        .expect("direct diagnostics must open");
    let diagnostics_cancel = diagnostics.cancel_handle();
    let (diag_tx, diag_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Some(snapshot) = diagnostics.recv() {
            if diag_tx.send(snapshot).is_err() {
                break;
            }
        }
    });

    let discovery_cancel = stage_direct_discovery(&engine, &pubkey.to_hex(), &relay);

    let subscription = engine
        .observe(
            LiveQuery::from_filter(direct_filter(&pubkey.to_hex(), QUERY_KIND)),
            None,
        )
        .expect("direct query must open");
    let query_cancel = subscription.cancel_handle();
    let (rows_tx, rows_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(batch) = subscription.recv() {
            if rows_tx.send(batch).is_err() {
                break;
            }
        }
    });
    let mut rows = BTreeMap::new();
    let rows_deadline = Instant::now() + WAIT;
    let evidence = loop {
        let frame = recv_before(&rows_rx, rows_deadline, "direct query");
        apply_direct_deltas(&mut rows, frame.deltas, &relay_url);
        let normalized = normalize_direct_evidence(frame.evidence, &relay_url);
        if rows.contains_key(&expected_row_id) && normalized == expected_limited_evidence() {
            break normalized;
        }
    };
    // Exact worker ownership (#235) may legitimately close this relay when
    // demand reaches zero. Keep both observations live until the two-filter
    // plan is visible and every admitted discovery/content response has
    // reached diagnostics. That equality barrier is the stable baseline;
    // only then may withdrawing discovery prove the handoff caused no replay.
    let handoff_baseline = wait_for_direct_handoff_quiescence(&diag_rx, &relay);
    discovery_cancel.cancel();

    let diagnostics_deadline = Instant::now() + WAIT;
    let mut last_diagnostics = None;
    let diagnostics = loop {
        let remaining = diagnostics_deadline.saturating_duration_since(Instant::now());
        let snapshot = diag_rx.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "direct diagnostics did not settle within the total {WAIT:?} bound: {error}; \
                 handoff baseline: {handoff_baseline:?}; last snapshot: {last_diagnostics:?}; \
                 relay query counts: discovery={}, content={}",
                relay.query_count_for_kind(Kind::RelayList.as_u16()),
                relay.query_count_for_kind(QUERY_KIND),
            )
        });
        let normalized = normalize_direct_diagnostics(snapshot, &relay_url);
        if content_phase_is_quiescent(&normalized, handoff_baseline, &relay) {
            break normalized;
        }
        last_diagnostics = Some(normalized);
    };
    assert_content_phase_diagnostics(&diagnostics, handoff_baseline, &relay, "direct");

    let unsigned = UnsignedEvent::new(
        pubkey,
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    );
    let receipt_rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("direct publish must enqueue");
    let receipts = collect_direct_receipts(receipt_rx, &relay_url);
    assert_eq!(
        receipts,
        expected_success_receipts(keys),
        "direct durable publish must expose the exact ordered \
         acceptance/sign/route/await-relay/send/ack facts"
    );

    query_cancel.cancel();
    diagnostics_cancel.cancel();
    engine.shutdown();
    relay.shutdown();

    ScenarioOutcome {
        rows: rows.into_values().collect(),
        evidence,
        receipts,
        diagnostics,
    }
}

fn collect_ffi_receipts(rx: &mpsc::Receiver<FfiWriteStatus>, relay: &str) -> Vec<NormStatus> {
    let mut statuses = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let status = recv_before(rx, deadline, "FFI receipt");
        let normalized = normalize_ffi_status(status, relay);
        let terminal = matches!(
            normalized,
            NormStatus::Acked(_)
                | NormStatus::Rejected(_, _)
                | NormStatus::GaveUp(_)
                | NormStatus::Failed(_)
        );
        statuses.push(normalized);
        if terminal {
            return statuses;
        }
    }
}

async fn run_ffi_success(keys: &Keys, query_event: &nostr::Event) -> ScenarioOutcome {
    let relay = setup_relay(keys, query_event).await;
    let expected_row_id = query_event.id.to_hex();
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        indexer_relays: vec![relay_url.clone()],
        app_relays: vec![],
        fallback_relays: vec![],
        // Same real-discovery opt-in as `run_direct_success` — the seeded
        // kind:10002 names this loopback relay, so it must be admitted past
        // the SSRF policy (issue #121) for the two facades to stay identical.
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("FFI engine must construct");
    let registration = engine
        .add_account(keys.secret_key().to_secret_hex())
        .expect("FFI account must register");
    let pubkey = registration.public_key();
    engine
        .set_active_account(Some(pubkey.clone()))
        .expect("FFI account must activate");

    let (diag_tx, diag_rx) = mpsc::channel();
    let diagnostics_handle = engine
        .observe_diagnostics(Box::new(FfiDiagnostics {
            tx: Mutex::new(diag_tx),
        }))
        .expect("FFI diagnostics must open");
    let discovery_handle = stage_ffi_discovery(&engine, &pubkey, &relay);
    let (rows_tx, rows_rx) = mpsc::channel();
    let query_handle = engine
        .observe(
            ffi_filter(&pubkey, QUERY_KIND),
            None,
            Box::new(FfiRows {
                tx: Mutex::new(rows_tx),
            }),
        )
        .expect("FFI query must open");
    let mut rows = BTreeMap::new();
    let rows_deadline = Instant::now() + WAIT;
    let evidence = loop {
        let (deltas, evidence) = recv_before(&rows_rx, rows_deadline, "FFI query");
        apply_ffi_deltas(&mut rows, deltas, &relay_url);
        let normalized = normalize_ffi_evidence(evidence, &relay_url);
        if rows.contains_key(&expected_row_id) && normalized == expected_limited_evidence() {
            break normalized;
        }
    };
    // Same drained, continuously-owned handoff proof as the direct facade.
    let handoff_baseline = wait_for_ffi_handoff_quiescence(&diag_rx, &relay);
    discovery_handle.cancel();

    let diagnostics_deadline = Instant::now() + WAIT;
    let mut last_diagnostics = None;
    let diagnostics = loop {
        let remaining = diagnostics_deadline.saturating_duration_since(Instant::now());
        let snapshot = diag_rx.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "FFI diagnostics did not settle within the total {WAIT:?} bound: {error}; \
                 handoff baseline: {handoff_baseline:?}; last snapshot: {last_diagnostics:?}; \
                 relay query counts: discovery={}, content={}",
                relay.query_count_for_kind(Kind::RelayList.as_u16()),
                relay.query_count_for_kind(QUERY_KIND),
            )
        });
        let normalized = normalize_ffi_diagnostics(snapshot, &relay_url);
        if content_phase_is_quiescent(&normalized, handoff_baseline, &relay) {
            break normalized;
        }
        last_diagnostics = Some(normalized);
    };
    assert_content_phase_diagnostics(&diagnostics, handoff_baseline, &relay, "FFI");

    let (receipt_tx, receipt_rx) = mpsc::channel();
    engine
        .publish(
            FfiWriteIntent {
                payload: FfiWritePayload::Unsigned {
                    pubkey,
                    created_at: WRITE_CREATED_AT,
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                },
                durability: FfiDurability::Durable,
                routing: FfiWriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(FfiReceipts {
                tx: Mutex::new(receipt_tx),
            }),
        )
        .expect("FFI publish must enqueue");
    let receipts = collect_ffi_receipts(&receipt_rx, &relay_url);
    assert_eq!(
        receipts,
        expected_success_receipts(keys),
        "FFI durable publish must expose the exact ordered \
         acceptance/sign/route/await-relay/send/ack facts"
    );

    query_handle.cancel();
    diagnostics_handle.cancel();
    engine.shutdown();
    relay.shutdown();

    ScenarioOutcome {
        rows: rows.into_values().collect(),
        evidence,
        receipts,
        diagnostics,
    }
}

/// #8 U2 fail-closed AUTH park, direct half. Same seeding/discovery
/// preamble as `run_direct_success` (reads are NOT gated by
/// `auth_required_writes`, so real NIP-65 discovery works unchanged) and
/// the identical engine construction/keys, but the relay answers the
/// unauthenticated durable EVENT with `["AUTH", challenge]` +
/// `["OK", id, false, "auth-required: ..."]`. No AUTH policy registry
/// exists at this wave, so the write must park on exactly one
/// `AwaitingAuth` beat and then stay silent — no retry, no terminal.
async fn run_direct_auth_parked(keys: &Keys, query_event: &nostr::Event) -> Vec<NormStatus> {
    let relay = ScriptedRelay::start(&RelayConfig {
        auth_required_writes: true,
        ..RelayConfig::default()
    })
    .await;
    relay.seed_own_relay_list(keys, QUERY_CREATED_AT - 1).await;
    relay.seed_signed_event(query_event).await;
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![relay_url.clone()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..EngineConfig::default()
    })
    .expect("direct auth-parked engine must construct");
    let pubkey = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("direct auth-parked account must register")
        .public_key();
    engine
        .set_active_account(Some(pubkey))
        .expect("direct auth-parked account must activate");

    let discovery_cancel = stage_direct_discovery(&engine, &pubkey.to_hex(), &relay);

    let unsigned = UnsignedEvent::new(
        pubkey,
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    );
    let receipt_rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("direct auth-parked publish must enqueue");
    let receipts = collect_direct_receipts_until_awaiting_auth(&receipt_rx, &relay_url);
    assert_eq!(
        receipt_rx.recv_timeout(Duration::from_secs(2)),
        Err(RecvTimeoutError::Timeout),
        "a fail-closed AUTH park must emit no further direct status: no retry, no terminal"
    );

    discovery_cancel.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

/// FFI half of the fail-closed AUTH park — its own isolated relay instance
/// and the identical engine construction/keys as `run_ffi_success`, so the
/// byte-identical comparison against the direct half is honest.
async fn run_ffi_auth_parked(keys: &Keys, query_event: &nostr::Event) -> Vec<NormStatus> {
    let relay = ScriptedRelay::start(&RelayConfig {
        auth_required_writes: true,
        ..RelayConfig::default()
    })
    .await;
    relay.seed_own_relay_list(keys, QUERY_CREATED_AT - 1).await;
    relay.seed_signed_event(query_event).await;
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        indexer_relays: vec![relay_url.clone()],
        app_relays: vec![],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("FFI auth-parked engine must construct");
    let registration = engine
        .add_account(keys.secret_key().to_secret_hex())
        .expect("FFI auth-parked account must register");
    let pubkey = registration.public_key();
    engine
        .set_active_account(Some(pubkey.clone()))
        .expect("FFI auth-parked account must activate");

    let discovery_handle = stage_ffi_discovery(&engine, &pubkey, &relay);

    let (receipt_tx, receipt_rx) = mpsc::channel();
    engine
        .publish(
            FfiWriteIntent {
                payload: FfiWritePayload::Unsigned {
                    pubkey,
                    created_at: WRITE_CREATED_AT,
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                },
                durability: FfiDurability::Durable,
                routing: FfiWriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(FfiReceipts {
                tx: Mutex::new(receipt_tx),
            }),
        )
        .expect("FFI auth-parked publish must enqueue");
    let receipts = collect_ffi_receipts_until_awaiting_auth(&receipt_rx, &relay_url);
    assert_eq!(
        receipt_rx.recv_timeout(Duration::from_secs(2)),
        Err(RecvTimeoutError::Timeout),
        "a fail-closed AUTH park must emit no further FFI status: no retry, no terminal"
    );

    discovery_handle.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

/// #47 Unit A override publish, direct half. The override pubkey is
/// registered as a SECONDARY account -- in the engine's signer set but
/// never active -- while the active account is a different registered
/// identity. Same seeding/discovery preamble as `run_direct_success`, but
/// the seeded kind:10002 belongs to the OVERRIDE identity: `AuthorOutbox`
/// routes by the intent's author, which #47 pins to the override. A silent
/// fallback to the active account would sign a DIFFERENT author and change
/// the deterministic event id the `Signed` receipt names.
async fn run_direct_override_publish(active: &Keys, override_keys: &Keys) -> Vec<NormStatus> {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay
        .seed_own_relay_list(override_keys, QUERY_CREATED_AT - 1)
        .await;
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![relay_url.clone()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..EngineConfig::default()
    })
    .expect("direct override engine must construct");
    let active_pubkey = engine
        .add_account(&active.secret_key().to_secret_hex())
        .expect("direct active account must register")
        .public_key();
    engine
        .set_active_account(Some(active_pubkey))
        .expect("direct active account must activate");
    let override_pubkey = engine
        .add_account(&override_keys.secret_key().to_secret_hex())
        .expect("direct override account must register as a secondary")
        .public_key();

    let discovery_cancel = stage_direct_discovery(&engine, &override_pubkey.to_hex(), &relay);

    let unsigned = UnsignedEvent::new(
        override_pubkey,
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    );
    let receipt_rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: Some(override_pubkey),
        })
        .expect("direct override publish must enqueue");
    let receipts = collect_direct_receipts(receipt_rx, &relay_url);

    discovery_cancel.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

/// FFI half of the override publish -- its own isolated relay instance and
/// the identical two-account construction as the direct half (active
/// account registered AND active, override registered but never active),
/// so the byte-identical receipt comparison is honest.
async fn run_ffi_override_publish(active: &Keys, override_keys: &Keys) -> Vec<NormStatus> {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay
        .seed_own_relay_list(override_keys, QUERY_CREATED_AT - 1)
        .await;
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        indexer_relays: vec![relay_url.clone()],
        app_relays: vec![],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("FFI override engine must construct");
    let active_pubkey = engine
        .add_account(active.secret_key().to_secret_hex())
        .expect("FFI active account must register")
        .public_key();
    engine
        .set_active_account(Some(active_pubkey))
        .expect("FFI active account must activate");
    let override_pubkey = engine
        .add_account(override_keys.secret_key().to_secret_hex())
        .expect("FFI override account must register as a secondary")
        .public_key();

    let discovery_handle = stage_ffi_discovery(&engine, &override_pubkey, &relay);

    let (receipt_tx, receipt_rx) = mpsc::channel();
    engine
        .publish(
            FfiWriteIntent {
                payload: FfiWritePayload::Unsigned {
                    pubkey: override_pubkey.clone(),
                    created_at: WRITE_CREATED_AT,
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                },
                durability: FfiDurability::Durable,
                routing: FfiWriteRouting::AuthorOutbox,
                identity_override: Some(override_pubkey),
            },
            Box::new(FfiReceipts {
                tx: Mutex::new(receipt_tx),
            }),
        )
        .expect("FFI override publish must enqueue");
    let receipts = collect_ffi_receipts(&receipt_rx, &relay_url);

    discovery_handle.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

async fn run_direct_tampered(keys: &Keys) -> TamperedOutcome {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay_url.clone()],
        ..EngineConfig::default()
    })
    .expect("direct tampered engine must construct");
    let mut event = nostr::EventBuilder::new(Kind::Custom(WRITE_KIND), "original")
        .custom_created_at(Timestamp::from(WRITE_CREATED_AT))
        .sign_with_keys(keys)
        .expect("tampered fixture must first sign cleanly");
    event.content = "tampered".to_string();
    let rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Signed(event),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("well-formed tampered input is accepted by the direct call boundary");
    let first = rx
        .recv_timeout(WAIT)
        .expect("tampered direct publish must fail on the receipt stream");
    let receipts = vec![normalize_direct_status(first, &relay_url)];
    assert!(
        matches!(receipts.as_slice(), [NormStatus::Failed(_)]),
        "tampered direct publish must be Failed-first: {receipts:?}"
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Err(RecvTimeoutError::Disconnected),
        "tampered direct publish must close after Failed; Timeout would leave later facts possible"
    );
    engine.shutdown();
    let relay_contact_count = relay.contact_count();
    relay.shutdown();
    TamperedOutcome {
        receipts,
        relay_contact_count,
    }
}

async fn run_ffi_tampered(keys: &Keys) -> TamperedOutcome {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        indexer_relays: vec![],
        app_relays: vec![relay_url.clone()],
        fallback_relays: vec![],
        ..NmpEngineConfig::default()
    })
    .expect("FFI tampered engine must construct");
    let event = nostr::EventBuilder::new(Kind::Custom(WRITE_KIND), "original")
        .custom_created_at(Timestamp::from(WRITE_CREATED_AT))
        .sign_with_keys(keys)
        .expect("tampered fixture must first sign cleanly");
    let (receipt_tx, receipt_rx) = mpsc::channel();
    engine
        .publish(
            FfiWriteIntent {
                payload: FfiWritePayload::Signed {
                    id: event.id.to_hex(),
                    pubkey: event.pubkey.to_hex(),
                    created_at: event.created_at.as_secs(),
                    kind: event.kind.as_u16(),
                    tags: event.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
                    content: "tampered".to_string(),
                    sig: event.sig.to_string(),
                },
                durability: FfiDurability::Durable,
                routing: FfiWriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(FfiReceipts {
                tx: Mutex::new(receipt_tx),
            }),
        )
        .expect("well-formed tampered input must parse at the FFI call boundary");
    let first = receipt_rx
        .recv_timeout(WAIT)
        .expect("tampered FFI publish must fail on the receipt stream");
    let receipts = vec![normalize_ffi_status(first, &relay_url)];
    assert!(
        matches!(receipts.as_slice(), [NormStatus::Failed(_)]),
        "tampered FFI publish must be Failed-first: {receipts:?}"
    );
    assert_eq!(
        receipt_rx.recv_timeout(WAIT),
        Err(RecvTimeoutError::Disconnected),
        "tampered FFI publish must close after Failed; Timeout would leave later facts possible"
    );
    engine.shutdown();
    let relay_contact_count = relay.contact_count();
    relay.shutdown();
    TamperedOutcome {
        receipts,
        relay_contact_count,
    }
}

// #99: PR #97's FFI reattach coverage stopped at a pure enum-mapping unit
// test -- structural code-sharing (`nmp-ffi` delegates to the same
// `nmp::Engine`) is not itself proof, exactly the discipline this whole
// harness exists to enforce (module doc). The two scenarios below drive
// `reattach_receipt` through BOTH entry points and assert identical
// outcomes AND identical replayed fact sequences: one for a LIVE retained
// receipt (`Attached`, replaying `Accepted`+`AwaitingCapability`), one for
// a genuinely TERMINAL retained receipt reached via a real ephemeral
// abandon-on-restart (`Attached`, replaying the terminal `Failed` fact).
// Neither needs a relay at all -- `Accepted`/`AwaitingCapability`/ephemeral
// abandonment are purely local acceptance/persistence facts, independent
// of wire delivery.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormReattach {
    Attached,
    NotFound,
    RetainedButUnreadable,
}

fn direct_reattach_outcome(value: &ReceiptReattachment) -> NormReattach {
    match value {
        ReceiptReattachment::Attached(_) => NormReattach::Attached,
        ReceiptReattachment::NotFound => NormReattach::NotFound,
        ReceiptReattachment::RetainedButUnreadable => NormReattach::RetainedButUnreadable,
    }
}

fn ffi_reattach_outcome(value: FfiReceiptReattachment) -> NormReattach {
    match value {
        FfiReceiptReattachment::Attached => NormReattach::Attached,
        FfiReceiptReattachment::NotFound => NormReattach::NotFound,
        FfiReceiptReattachment::RetainedButUnreadable => NormReattach::RetainedButUnreadable,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReattachProof {
    outcome: NormReattach,
    replay: Vec<NormStatus>,
    /// A bogus id's reattach on the SAME (still-open) engine, proven
    /// alongside the real one so both surfaces exercise the shared
    /// `NotFound` path from the same live engine instance.
    unknown_id_outcome: NormReattach,
}

/// LIVE half: publish a durable Unsigned intent authored by an account that
/// is ACTIVE but has no registered signer (so it settles into a genuinely
/// retained `Accepted`+`AwaitingCapability` steady state, never resolving
/// further), then reattach with a second, independent observer and prove it
/// replays the identical fact sequence the original saw.
async fn run_direct_reattach_live() -> ReattachProof {
    let keys = Keys::generate();
    let engine = Engine::new(EngineConfig::default()).expect("direct engine must construct");
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("direct account must activate");

    let unsigned = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(REATTACH_LIVE_KIND),
        vec![],
        "reattach-live",
    );
    let tracked = engine
        .publish_tracked(WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("direct publish must enqueue");

    let deadline = Instant::now() + WAIT;
    assert_eq!(
        recv_before(&tracked.statuses, deadline, "direct original Accepted"),
        WriteStatus::Accepted
    );
    assert_eq!(
        recv_before(
            &tracked.statuses,
            deadline,
            "direct original AwaitingCapability"
        ),
        WriteStatus::AwaitingCapability {
            pubkey: keys.public_key()
        }
    );

    let outcome = engine
        .reattach_receipt(tracked.id)
        .expect("direct reattach call must succeed while the engine is open");
    let norm_outcome = direct_reattach_outcome(&outcome);
    let replay = match outcome {
        ReceiptReattachment::Attached(rx) => {
            let deadline = Instant::now() + WAIT;
            vec![
                normalize_direct_status(
                    recv_before(&rx, deadline, "direct replay Accepted"),
                    "n/a",
                ),
                normalize_direct_status(
                    recv_before(&rx, deadline, "direct replay AwaitingCapability"),
                    "n/a",
                ),
            ]
        }
        _ => panic!("expected Attached for a live retained receipt, got {norm_outcome:?}"),
    };

    let unknown_id_outcome = direct_reattach_outcome(
        &engine
            .reattach_receipt(ReceiptId(u64::MAX))
            .expect("direct reattach call must succeed while the engine is open"),
    );

    engine.shutdown();
    ReattachProof {
        outcome: norm_outcome,
        replay,
        unknown_id_outcome,
    }
}

async fn run_ffi_reattach_live() -> ReattachProof {
    let keys = Keys::generate();
    let engine = NmpEngine::new(NmpEngineConfig::default()).expect("FFI engine must construct");
    engine
        .set_active_account(Some(keys.public_key().to_hex()))
        .expect("FFI account must activate");

    let (tx, rx) = mpsc::channel();
    let observer = Box::new(FfiReceipts { tx: Mutex::new(tx) });
    let receipt_id = engine
        .publish(
            FfiWriteIntent {
                payload: FfiWritePayload::Unsigned {
                    pubkey: keys.public_key().to_hex(),
                    created_at: WRITE_CREATED_AT,
                    kind: REATTACH_LIVE_KIND,
                    tags: vec![],
                    content: "reattach-live".to_string(),
                },
                durability: FfiDurability::Durable,
                routing: FfiWriteRouting::AuthorOutbox,
                identity_override: None,
            },
            observer,
        )
        .expect("FFI publish must enqueue");

    let deadline = Instant::now() + WAIT;
    assert_eq!(
        normalize_ffi_status(recv_before(&rx, deadline, "FFI original Accepted"), "n/a"),
        NormStatus::Accepted
    );
    assert_eq!(
        normalize_ffi_status(
            recv_before(&rx, deadline, "FFI original AwaitingCapability"),
            "n/a"
        ),
        NormStatus::AwaitingCapability(keys.public_key().to_hex())
    );

    let (replay_tx, replay_rx) = mpsc::channel();
    let replay_observer = Box::new(FfiReceipts {
        tx: Mutex::new(replay_tx),
    });
    let outcome = engine
        .reattach_receipt(receipt_id, replay_observer)
        .expect("FFI reattach call must succeed while the engine is open");
    let norm_outcome = ffi_reattach_outcome(outcome);
    let replay = match outcome {
        FfiReceiptReattachment::Attached => {
            let deadline = Instant::now() + WAIT;
            vec![
                normalize_ffi_status(
                    recv_before(&replay_rx, deadline, "FFI replay Accepted"),
                    "n/a",
                ),
                normalize_ffi_status(
                    recv_before(&replay_rx, deadline, "FFI replay AwaitingCapability"),
                    "n/a",
                ),
            ]
        }
        other => panic!("expected Attached for a live retained receipt, got {other:?}"),
    };

    let (unknown_tx, unknown_rx) = mpsc::channel();
    let unknown_observer = Box::new(FfiReceipts {
        tx: Mutex::new(unknown_tx),
    });
    let unknown_id_outcome = ffi_reattach_outcome(
        engine
            .reattach_receipt(u64::MAX, unknown_observer)
            .expect("FFI reattach call must succeed while the engine is open"),
    );
    assert_eq!(
        unknown_rx.try_recv(),
        Err(mpsc::TryRecvError::Disconnected),
        "an unknown-id reattach must spawn no forwarding thread"
    );

    engine.shutdown();
    ReattachProof {
        outcome: norm_outcome,
        replay,
        unknown_id_outcome,
    }
}

/// TERMINAL half: publish an EPHEMERAL intent authored by an active account
/// with no registered signer, so it durably persists as a receipt-only
/// (`intent_id: None`) row still `Accepted` at shutdown time. Reopening the
/// SAME `store_path` runs `RedbStore::open`'s own boot-time reconciliation
/// (`reconcile_ephemeral_receipts_in_txn`), which abandons any such row --
/// a real, publicly-reachable "terminal retained receipt" with no internal
/// `EngineMsg`/`CancelWrite` reach-in needed (that verb is not on the
/// supported facade surface at all).
async fn run_direct_reattach_terminal(path: &std::path::Path) -> ReattachProof {
    let keys = Keys::generate();
    let receipt_id = {
        let engine = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .expect("direct engine must construct");
        engine
            .set_active_account(Some(keys.public_key()))
            .expect("direct account must activate");
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(WRITE_CREATED_AT),
            Kind::Custom(REATTACH_TERMINAL_KIND),
            vec![],
            "reattach-terminal",
        );
        let tracked = engine
            .publish_tracked(WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Ephemeral,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            })
            .expect("direct ephemeral publish must enqueue");
        let deadline = Instant::now() + WAIT;
        assert_eq!(
            recv_before(
                &tracked.statuses,
                deadline,
                "direct terminal-setup Accepted"
            ),
            WriteStatus::Accepted
        );
        engine.shutdown();
        tracked.id
    };

    let engine = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("direct engine must reopen over the same store");
    let outcome = engine
        .reattach_receipt(receipt_id)
        .expect("direct reattach call must succeed while the engine is open");
    let norm_outcome = direct_reattach_outcome(&outcome);
    let replay = match outcome {
        ReceiptReattachment::Attached(rx) => {
            let deadline = Instant::now() + WAIT;
            vec![normalize_direct_status(
                recv_before(&rx, deadline, "direct terminal replay"),
                "n/a",
            )]
        }
        _ => panic!("expected Attached for an abandoned terminal receipt, got {norm_outcome:?}"),
    };
    let unknown_id_outcome = direct_reattach_outcome(
        &engine
            .reattach_receipt(ReceiptId(u64::MAX))
            .expect("direct reattach call must succeed while the engine is open"),
    );
    engine.shutdown();
    ReattachProof {
        outcome: norm_outcome,
        replay,
        unknown_id_outcome,
    }
}

async fn run_ffi_reattach_terminal(path: &std::path::Path) -> ReattachProof {
    let keys = Keys::generate();
    let receipt_id = {
        let engine = NmpEngine::new(NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        })
        .expect("FFI engine must construct");
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .expect("FFI account must activate");
        let (tx, rx) = mpsc::channel();
        let observer = Box::new(FfiReceipts { tx: Mutex::new(tx) });
        let receipt_id = engine
            .publish(
                FfiWriteIntent {
                    payload: FfiWritePayload::Unsigned {
                        pubkey: keys.public_key().to_hex(),
                        created_at: WRITE_CREATED_AT,
                        kind: REATTACH_TERMINAL_KIND,
                        tags: vec![],
                        content: "reattach-terminal".to_string(),
                    },
                    durability: FfiDurability::Ephemeral,
                    routing: FfiWriteRouting::AuthorOutbox,
                    identity_override: None,
                },
                observer,
            )
            .expect("FFI ephemeral publish must enqueue");
        let deadline = Instant::now() + WAIT;
        assert_eq!(
            normalize_ffi_status(
                recv_before(&rx, deadline, "FFI terminal-setup Accepted"),
                "n/a"
            ),
            NormStatus::Accepted
        );
        engine.shutdown();
        receipt_id
    };

    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..NmpEngineConfig::default()
    })
    .expect("FFI engine must reopen over the same store");
    let (replay_tx, replay_rx) = mpsc::channel();
    let replay_observer = Box::new(FfiReceipts {
        tx: Mutex::new(replay_tx),
    });
    let outcome = engine
        .reattach_receipt(receipt_id, replay_observer)
        .expect("FFI reattach call must succeed while the engine is open");
    let norm_outcome = ffi_reattach_outcome(outcome);
    let replay = match outcome {
        FfiReceiptReattachment::Attached => {
            let deadline = Instant::now() + WAIT;
            vec![normalize_ffi_status(
                recv_before(&replay_rx, deadline, "FFI terminal replay"),
                "n/a",
            )]
        }
        other => panic!("expected Attached for an abandoned terminal receipt, got {other:?}"),
    };
    let (unknown_tx, unknown_rx) = mpsc::channel();
    let unknown_observer = Box::new(FfiReceipts {
        tx: Mutex::new(unknown_tx),
    });
    let unknown_id_outcome = ffi_reattach_outcome(
        engine
            .reattach_receipt(u64::MAX, unknown_observer)
            .expect("FFI reattach call must succeed while the engine is open"),
    );
    assert_eq!(
        unknown_rx.try_recv(),
        Err(mpsc::TryRecvError::Disconnected),
        "an unknown-id reattach must spawn no forwarding thread"
    );

    engine.shutdown();
    ReattachProof {
        outcome: norm_outcome,
        replay,
        unknown_id_outcome,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_reattach_are_semantically_identical_for_a_live_retained_receipt() {
    let direct = run_direct_reattach_live().await;
    let ffi = run_ffi_reattach_live().await;
    assert_eq!(
        direct, ffi,
        "direct and FFI reattach must expose identical outcomes, identical replayed receipt \
         facts, and identical unknown-id NotFound behavior"
    );
    assert_eq!(direct.outcome, NormReattach::Attached);
    assert_eq!(direct.unknown_id_outcome, NormReattach::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_reattach_are_semantically_identical_for_a_terminal_retained_receipt() {
    let direct_dir = tempfile::tempdir().expect("direct tempdir");
    let ffi_dir = tempfile::tempdir().expect("FFI tempdir");
    let direct = run_direct_reattach_terminal(&direct_dir.path().join("direct.redb")).await;
    let ffi = run_ffi_reattach_terminal(&ffi_dir.path().join("ffi.redb")).await;
    assert_eq!(
        direct, ffi,
        "direct and FFI reattach must expose identical outcomes and identical replayed terminal \
         facts for an ephemeral receipt abandoned on restart"
    );
    assert_eq!(direct.outcome, NormReattach::Attached);
    assert_eq!(
        direct.replay,
        vec![NormStatus::Failed(
            "ephemeral write abandoned after restart".to_string()
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_facades_are_semantically_identical_over_real_loopback() {
    let keys = fixed_keys();
    let query_event = nostr::EventBuilder::new(Kind::Custom(QUERY_KIND), "parity-row")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT))
        .sign_with_keys(&keys)
        .expect("parity row fixture must sign cleanly");
    let direct = run_direct_success(&keys, &query_event).await;
    let ffi = run_ffi_success(&keys, &query_event).await;
    assert_eq!(
        direct, ffi,
        "the direct and FFI facades must expose identical rows, AcquisitionEvidence, ordered \
         receipt facts, and DiagnosticsSnapshot shape"
    );

    let direct_tampered = run_direct_tampered(&keys).await;
    let ffi_tampered = run_ffi_tampered(&keys).await;
    assert_eq!(direct_tampered, ffi_tampered);
    assert_eq!(
        direct_tampered.relay_contact_count, 0,
        "tampered Signed input must fail before any REQ/EVENT reaches the relay"
    );
}

/// #8 U2: against a relay that actually challenges (NIP-42 write gating),
/// the durable write parks fail-closed — the relay's
/// `OK false "auth-required:"` yields exactly one `AwaitingAuth` beat and
/// the lane stays parked (no policy registry exists until Wave 3). That
/// park must be byte-identical between the direct Rust facade and the FFI
/// facade, and neither side may emit anything after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_required_relay_parks_write_identically_direct_and_ffi() {
    let keys = fixed_keys();
    let query_event = nostr::EventBuilder::new(Kind::Custom(QUERY_KIND), "parity-row")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT))
        .sign_with_keys(&keys)
        .expect("parity row fixture must sign cleanly");

    let direct = run_direct_auth_parked(&keys, &query_event).await;
    let ffi = run_ffi_auth_parked(&keys, &query_event).await;

    assert_eq!(
        direct, ffi,
        "the direct and FFI facades must expose the identical ordered fail-closed AUTH park"
    );
    assert_eq!(
        direct,
        expected_auth_parked_receipts(&keys),
        "a protected durable write must park on exactly \
         [Accepted, Signed, Routed, AwaitingRelay, Sent, AwaitingAuth]"
    );
}

/// #47 Unit A: a per-write `identity_override` naming a registered
/// SECONDARY account (not the active one) must observe the same semantics
/// through the direct Rust facade and the FFI facade: accepted, signed BY
/// THE OVERRIDE, routed via the override's own outbox, and acked. The
/// `Signed` receipt's event id is the author proof -- an id hashes the
/// author pubkey, so `expected_success_receipts(&override_keys)` can only
/// match if `event.pubkey` IS the override; a silent fallback to the active
/// account on either surface would mint a different id and fail both
/// comparisons.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_override_publish_signs_as_the_override_identically_direct_and_ffi() {
    let active = fixed_keys();
    let override_keys = Keys::generate();

    let direct = run_direct_override_publish(&active, &override_keys).await;
    let ffi = run_ffi_override_publish(&active, &override_keys).await;

    assert_eq!(
        direct, ffi,
        "the direct and FFI facades must expose identical ordered override-publish receipts"
    );
    assert_eq!(
        direct,
        expected_success_receipts(&override_keys),
        "an override publish must sign as the OVERRIDE author -- the Signed receipt must carry \
         the deterministic id of the override-authored event, never the active account's"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_follow_actions_are_identical_over_real_loopback() {
    let author = fixed_keys();
    let existing = Keys::generate();
    let target = Keys::generate();

    let direct = run_direct_follow_scenario(&author, &existing, &target).await;
    let ffi = run_ffi_follow_scenario(&author, &existing, &target).await;
    assert_eq!(
        direct, ffi,
        "the iOS FFI path and direct NMP path must expose the same relationship snapshots, \
         no-op semantics, and ordered follow/unfollow receipts"
    );

    assert_eq!(direct.initial.relationship, "not_following");
    assert_eq!(direct.initial.availability, "ready");
    assert_eq!(direct.after_follow.relationship, "following");
    assert_eq!(direct.after_unfollow.relationship, "not_following");
    assert_eq!(direct.preserved_existing_follow.relationship, "following");
    assert_eq!(
        direct.no_change,
        vec![
            NormFollowActionStatus::Acquiring,
            NormFollowActionStatus::NoChange(true)
        ]
    );
    // #8 U2: `FollowActionStatus::Receipt` forwards every underlying
    // `WriteStatus` fact verbatim, so both durable kind:3 writes carry the
    // deterministic cold-Nip42-session `awaiting_relay` beat between
    // `routed` and `sent` (see `expected_send_preamble`) — the unfollow too,
    // because worker reconciliation closed the write session when the
    // follow write acked.
    assert!(matches!(
        direct.follow.as_slice(),
        [
            NormFollowActionStatus::Acquiring,
            NormFollowActionStatus::Receipt("accepted"),
            NormFollowActionStatus::Receipt("signed"),
            NormFollowActionStatus::Receipt("routed"),
            NormFollowActionStatus::Receipt("awaiting_relay"),
            NormFollowActionStatus::Receipt("awaiting_auth"),
            NormFollowActionStatus::Receipt("sent"),
            NormFollowActionStatus::Receipt("acked")
        ]
    ));
    assert!(matches!(
        direct.unfollow.as_slice(),
        [
            NormFollowActionStatus::Acquiring,
            NormFollowActionStatus::Receipt("accepted"),
            NormFollowActionStatus::Receipt("signed"),
            NormFollowActionStatus::Receipt("routed"),
            NormFollowActionStatus::Receipt("awaiting_relay"),
            NormFollowActionStatus::Receipt("awaiting_auth"),
            NormFollowActionStatus::Receipt("sent"),
            NormFollowActionStatus::Receipt("acked")
        ]
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_follow_refuse_a_reconciled_missing_contact_list() {
    let author = fixed_keys();
    let target = Keys::generate();

    let direct = run_direct_missing_contact_list(&author, &target).await;
    let ffi = run_ffi_missing_contact_list(&author, &target).await;
    assert_eq!(
        direct, ffi,
        "direct Rust and the iOS FFI path must expose the same non-destructive missing-list state"
    );
    assert_eq!(direct.0.relationship, "not_following");
    assert_eq!(direct.0.availability, "no_contact_list");
    assert!(!direct.0.has_base);
    assert_eq!(
        direct.1,
        vec![
            NormFollowActionStatus::Acquiring,
            NormFollowActionStatus::Failed("NoContactList".to_string())
        ],
        "ordinary follow must publish nothing when there is no established kind:3 base"
    );
}
