//! #52 Unit D: execute one content-neutral loopback scenario through the
//! supported direct Rust facade and through `nmp-ffi`, then compare the
//! semantic observations. Each run gets an isolated instance of the SAME
//! `nmp-bdd::relays::ScriptedRelay`; no second relay fake lives here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nmp::{
    AcquisitionEvidence, AuthPhase, Binding, DiagnosticsSnapshot, Durability, Engine, EngineConfig,
    Filter, Lane, LiveQuery, RowDelta, ShortfallFact, SourceStatus, Timestamp, UnsignedEvent,
    WriteIntent, WritePayload, WriteRouting, WriteStatus,
};
use nmp_bdd::relays::{RelayConfig, ScriptedRelay};
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::observer::{DiagnosticsObserver, ReceiptObserver, RowObserver};
use nmp_ffi::types::{
    FfiAcquisitionEvidence, FfiAuthPhase, FfiBinding, FfiDiagnosticsSnapshot, FfiDurability,
    FfiFilter, FfiRowDelta, FfiShortfallFact, FfiSourceStatus, FfiWriteIntent, FfiWritePayload,
    FfiWriteRouting, FfiWriteStatus,
};
use nostr::{JsonUtil, Keys, Kind};

const WAIT: Duration = Duration::from_secs(10);
const DISCOVERY_TRIGGER_KIND: u16 = 9_997;
const QUERY_KIND: u16 = 9_998;
const WRITE_KIND: u16 = 9_999;
const QUERY_CREATED_AT: u64 = 1_700_000_100;
const WRITE_CREATED_AT: u64 = 1_700_000_200;
const SECRET_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormRow {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
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
    AwaitingCapability,
    Signed(String),
    Routed(Vec<String>),
    Sent(String),
    Acked(String),
    Rejected(String, String),
    GaveUp(String),
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
    }
}

fn direct_status_name(status: SourceStatus) -> String {
    match status {
        SourceStatus::Requesting => "requesting".to_string(),
        SourceStatus::Connecting => "connecting".to_string(),
        SourceStatus::Disconnected => "disconnected".to_string(),
        SourceStatus::AwaitingAuth { phase } => match phase {
            AuthPhase::AwaitingPolicy => "awaiting_auth:policy".to_string(),
            AuthPhase::AwaitingSignature => "awaiting_auth:signature".to_string(),
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
            FfiAuthPhase::AwaitingSignature => "awaiting_auth:signature".to_string(),
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
        WriteStatus::AwaitingCapability => NormStatus::AwaitingCapability,
        WriteStatus::Signed(id) => NormStatus::Signed(id.to_hex()),
        WriteStatus::Routed(relays) => NormStatus::Routed(
            relays
                .iter()
                .map(|url| normalize_url(url.as_str(), relay))
                .collect(),
        ),
        WriteStatus::Sent(url) => NormStatus::Sent(normalize_url(url.as_str(), relay)),
        WriteStatus::Acked(url) => NormStatus::Acked(normalize_url(url.as_str(), relay)),
        WriteStatus::Rejected(url, reason) => {
            NormStatus::Rejected(normalize_url(url.as_str(), relay), reason)
        }
        WriteStatus::GaveUp(url) => NormStatus::GaveUp(normalize_url(url.as_str(), relay)),
        WriteStatus::Failed(reason) => NormStatus::Failed(reason),
    }
}

fn normalize_ffi_status(status: FfiWriteStatus, relay: &str) -> NormStatus {
    match status {
        FfiWriteStatus::Accepted => NormStatus::Accepted,
        FfiWriteStatus::AwaitingCapability => NormStatus::AwaitingCapability,
        FfiWriteStatus::Signed { event_id } => NormStatus::Signed(event_id),
        FfiWriteStatus::Routed { mut relays } => {
            for url in &mut relays {
                *url = normalize_url(url, relay);
            }
            relays.sort();
            NormStatus::Routed(relays)
        }
        FfiWriteStatus::Sent { relay: url } => NormStatus::Sent(normalize_url(&url, relay)),
        FfiWriteStatus::Acked { relay: url } => NormStatus::Acked(normalize_url(&url, relay)),
        FfiWriteStatus::Rejected { relay: url, reason } => {
            NormStatus::Rejected(normalize_url(&url, relay), reason)
        }
        FfiWriteStatus::GaveUp { relay: url } => NormStatus::GaveUp(normalize_url(&url, relay)),
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

fn direct_row(row: &nostr::Event) -> NormRow {
    NormRow {
        id: row.id.to_hex(),
        pubkey: row.pubkey.to_hex(),
        created_at: row.created_at.as_secs(),
        kind: row.kind.as_u16(),
        tags: row.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
        content: row.content.clone(),
        sig: row.sig.to_string(),
    }
}

fn apply_direct_deltas(rows: &mut BTreeMap<String, NormRow>, deltas: Vec<RowDelta>) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) => {
                let normalized = direct_row(&row);
                rows.insert(normalized.id.clone(), normalized);
            }
            RowDelta::Removed(id) => {
                rows.remove(&id.to_hex());
            }
        }
    }
}

fn apply_ffi_deltas(rows: &mut BTreeMap<String, NormRow>, deltas: Vec<FfiRowDelta>) {
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
                };
                rows.insert(normalized.id.clone(), normalized);
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

fn preflight_is_quiescent(
    snapshot: &NormDiagnostics,
    relay_discovery_query_count: u64,
) -> Option<u64> {
    let [relay] = snapshot.relays.as_slice() else {
        return None;
    };
    let has_preflight = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, DISCOVERY_TRIGGER_KIND));
    let has_internal_discovery = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, Kind::RelayList.as_u16()));
    let routed_through_nip65 = relay
        .by_lane
        .iter()
        .any(|(lane, count)| lane == "nip65_write" && *count > 0);
    let baseline = event_count(relay, Kind::RelayList.as_u16());
    (has_preflight
        && !has_internal_discovery
        && routed_through_nip65
        && relay_discovery_query_count >= 1
        && baseline == relay_discovery_query_count)
        .then_some(baseline)
}

fn content_phase_is_quiescent(
    snapshot: &NormDiagnostics,
    nip65_baseline: u64,
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
        && content_req_count >= 1
        && discovery_req_count == nip65_baseline
        && event_count(relay, QUERY_KIND) == 1
        && event_count(relay, Kind::RelayList.as_u16()) == discovery_req_count
        && !relay.coverage.is_empty()
        && relay
            .coverage
            .iter()
            .all(|(_, coverage)| coverage.is_none())
}

fn assert_content_phase_diagnostics(
    snapshot: &NormDiagnostics,
    nip65_baseline: u64,
    relay: &ScriptedRelay,
    surface: &str,
) {
    assert!(
        content_phase_is_quiescent(snapshot, nip65_baseline, relay),
        "{surface} diagnostics must contain only the discovered NIP-65-routed content plan, \
         exactly one content event, and an unchanged discovery-event baseline: {snapshot:?}"
    );
}

fn wait_for_direct_preflight_quiescence(
    rx: &mpsc::Receiver<DiagnosticsSnapshot>,
    relay: &ScriptedRelay,
) -> u64 {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = recv_before(rx, deadline, "direct preflight diagnostics");
        let snapshot = normalize_direct_diagnostics(snapshot, relay.url.as_str());
        if let Some(baseline) = preflight_is_quiescent(
            &snapshot,
            relay.query_count_for_kind(Kind::RelayList.as_u16()),
        ) {
            return baseline;
        }
    }
}

fn wait_for_ffi_preflight_quiescence(
    rx: &mpsc::Receiver<FfiDiagnosticsSnapshot>,
    relay: &ScriptedRelay,
) -> u64 {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = recv_before(rx, deadline, "FFI preflight diagnostics");
        let snapshot = normalize_ffi_diagnostics(snapshot, relay.url.as_str());
        if let Some(baseline) = preflight_is_quiescent(
            &snapshot,
            relay.query_count_for_kind(Kind::RelayList.as_u16()),
        ) {
            return baseline;
        }
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

fn expected_success_receipts(keys: &Keys) -> Vec<NormStatus> {
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
        NormStatus::Sent(relay.clone()),
        NormStatus::Acked(relay),
    ]
}

struct FfiRows {
    tx: Mutex<mpsc::Sender<(Vec<FfiRowDelta>, FfiAcquisitionEvidence)>>,
}

impl RowObserver for FfiRows {
    fn on_batch(&self, deltas: Vec<FfiRowDelta>, evidence: FfiAcquisitionEvidence) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send((deltas, evidence));
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
    diagnostics: &mpsc::Receiver<DiagnosticsSnapshot>,
) -> u64 {
    let subscription = engine
        .observe(LiveQuery(direct_filter(pubkey, DISCOVERY_TRIGGER_KIND)))
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
        let (_deltas, evidence) = recv_before(&rx, deadline, "direct discovery query");
        let evidence = normalize_direct_evidence(evidence, relay.url.as_str());
        if evidence == expected_limited_evidence() {
            break;
        }
    }
    let baseline = wait_for_direct_preflight_quiescence(diagnostics, relay);
    cancel.cancel();
    baseline
}

fn stage_ffi_discovery(
    engine: &NmpEngine,
    pubkey: &str,
    relay: &ScriptedRelay,
    diagnostics: &mpsc::Receiver<FfiDiagnosticsSnapshot>,
) -> u64 {
    let (tx, rx) = mpsc::channel();
    let handle = engine
        .observe(
            ffi_filter(pubkey, DISCOVERY_TRIGGER_KIND),
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
    let baseline = wait_for_ffi_preflight_quiescence(diagnostics, relay);
    handle.cancel();
    baseline
}

impl ReceiptObserver for FfiReceipts {
    fn on_status(&self, status: FfiWriteStatus) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .send(status);
    }
}

async fn setup_relay(keys: &Keys, query_event: &nostr::Event) -> ScriptedRelay {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    relay.seed_own_relay_list(keys, QUERY_CREATED_AT - 1).await;
    relay.seed_signed_event(query_event).await;
    relay
}

async fn run_direct_success(keys: &Keys, query_event: &nostr::Event) -> ScenarioOutcome {
    let relay = setup_relay(keys, query_event).await;
    let expected_row_id = query_event.id.to_hex();
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![relay_url.clone()],
        ..EngineConfig::default()
    })
    .expect("direct engine must construct");
    let pubkey = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("direct account must register");
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

    let nip65_baseline = stage_direct_discovery(&engine, &pubkey.to_hex(), &relay, &diag_rx);

    let subscription = engine
        .observe(LiveQuery(direct_filter(&pubkey.to_hex(), QUERY_KIND)))
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
        let (deltas, evidence) = recv_before(&rows_rx, rows_deadline, "direct query");
        apply_direct_deltas(&mut rows, deltas);
        let normalized = normalize_direct_evidence(evidence, &relay_url);
        if rows.contains_key(&expected_row_id) && normalized == expected_limited_evidence() {
            break normalized;
        }
    };

    let diagnostics_deadline = Instant::now() + WAIT;
    let diagnostics = loop {
        let snapshot = recv_before(&diag_rx, diagnostics_deadline, "direct diagnostics");
        let normalized = normalize_direct_diagnostics(snapshot, &relay_url);
        if content_phase_is_quiescent(&normalized, nip65_baseline, &relay) {
            break normalized;
        }
    };
    assert_content_phase_diagnostics(&diagnostics, nip65_baseline, &relay, "direct");

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
        })
        .expect("direct publish must enqueue");
    let receipts = collect_direct_receipts(receipt_rx, &relay_url);
    assert_eq!(
        receipts,
        expected_success_receipts(keys),
        "direct durable publish must expose the exact ordered acceptance/sign/route/send/ack facts"
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
    })
    .expect("FFI engine must construct");
    let pubkey = engine
        .add_account(keys.secret_key().to_secret_hex())
        .expect("FFI account must register");
    engine
        .set_active_account(Some(pubkey.clone()))
        .expect("FFI account must activate");

    let (diag_tx, diag_rx) = mpsc::channel();
    let diagnostics_handle = engine
        .observe_diagnostics(Box::new(FfiDiagnostics {
            tx: Mutex::new(diag_tx),
        }))
        .expect("FFI diagnostics must open");
    let nip65_baseline = stage_ffi_discovery(&engine, &pubkey, &relay, &diag_rx);
    let (rows_tx, rows_rx) = mpsc::channel();
    let query_handle = engine
        .observe(
            ffi_filter(&pubkey, QUERY_KIND),
            Box::new(FfiRows {
                tx: Mutex::new(rows_tx),
            }),
        )
        .expect("FFI query must open");

    let mut rows = BTreeMap::new();
    let rows_deadline = Instant::now() + WAIT;
    let evidence = loop {
        let (deltas, evidence) = recv_before(&rows_rx, rows_deadline, "FFI query");
        apply_ffi_deltas(&mut rows, deltas);
        let normalized = normalize_ffi_evidence(evidence, &relay_url);
        if rows.contains_key(&expected_row_id) && normalized == expected_limited_evidence() {
            break normalized;
        }
    };

    let diagnostics_deadline = Instant::now() + WAIT;
    let diagnostics = loop {
        let snapshot = recv_before(&diag_rx, diagnostics_deadline, "FFI diagnostics");
        let normalized = normalize_ffi_diagnostics(snapshot, &relay_url);
        if content_phase_is_quiescent(&normalized, nip65_baseline, &relay) {
            break normalized;
        }
    };
    assert_content_phase_diagnostics(&diagnostics, nip65_baseline, &relay, "FFI");

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
        "FFI durable publish must expose the exact ordered acceptance/sign/route/send/ack facts"
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
