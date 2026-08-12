//! #52 Unit D: execute one content-neutral loopback scenario through the
//! supported direct Rust facade and through `nmp-ffi`, then compare the
//! semantic observations. Each run gets an isolated instance of the SAME
//! `nmp-test-support::relays::ScriptedRelay`; no second relay fake lives here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use nmp::{
    AcquisitionEvidence, AuthDenialSource, AuthPhase, Binding, CancelWriteOutcome,
    CorrelationToken, DiagnosticsSnapshot, Engine, EngineConfig, EngineError, FifoReceiver,
    FifoRecvTimeoutError, Filter, Identity, Lane, LiveQuery, NotSentReason, ReceiptId,
    ReceiptReattachment, RefuseReason, RelayState, RelayWaiting, RetryCause, Row, RowDelta,
    ShortfallFact, SigningState, SourceStatus, StalledWriteStage, Subscription, Timestamp,
    UnsignedEvent, WriteFact, WriteIntent, WriteOutcome, WritePayload, WriteRouting,
};
use nmp_ffi::convert::{write_status_to_ffi, FfiError, WriteStatusRef};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
// #680: observers/callbacks are gone; the facade exposes pull-based async
// stream objects whose `next()` we bridge into the existing mpsc drains via a
// forwarding Tokio task (all parity tests are `#[tokio::test(multi_thread)]`).
use nmp_ffi::facade::{
    FfiNip65Config, NmpDiagnosticsStream, NmpEngine, NmpEngineConfig, NmpReceiptStream,
    NmpRowStream,
};
use nmp_ffi::nip02::{
    FfiFollowActionStatus, FfiFollowAvailability, FfiFollowRelationship, FfiFollowSnapshot,
    NmpFollowActionStream, NmpFollowStream,
};
use nmp_ffi::types::{
    FfiAcquisitionEvidence, FfiAuthDenialSource, FfiAuthPhase, FfiBinding, FfiCancelWriteOutcome,
    FfiDiagnosticsSnapshot, FfiFilter, FfiIdentity, FfiNotSentReason, FfiReceiptReattachment,
    FfiRefuseReason, FfiRelayState, FfiRelayWaiting, FfiRetryCause, FfiRowDelta, FfiShortfallFact,
    FfiSigningState, FfiSourceStatus, FfiStalledWriteStage, FfiWriteFact, FfiWriteIntent,
    FfiWriteOutcome, FfiWritePayload, FfiWriteRouting,
};
use nmp_nip02::{
    observe_following, set_following, FollowAction, FollowActionStatus, FollowAvailability,
    FollowChange, FollowObservation, FollowRelationship, FollowSnapshot,
};
use nostr::PublicKey;
use nostr::{JsonUtil, Keys, Kind};

const WAIT: Duration = Duration::from_secs(10);
const SOURCE_ANCHOR_KIND: u16 = 9_997;
const QUERY_KIND: u16 = 9_998;
const WRITE_KIND: u16 = 9_999;
const REATTACH_LIVE_KIND: u16 = 9_996;
// Replaceable (NIP-01 10000..20000): the terminal-reattach scenario
// reaches its terminal through supersession at one coordinate.
const REATTACH_TERMINAL_KIND: u16 = 10_009;
const QUERY_CREATED_AT: u64 = 1_700_000_100;
const WRITE_CREATED_AT: u64 = 1_700_000_200;
const SECRET_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const PARITY_INDEXER: &str = "wss://indexer.example";

fn direct_nip65_config() -> EngineConfig {
    EngineConfig {
        indexer_relays: vec![PARITY_INDEXER.to_string()],
        ..EngineConfig::default()
    }
}

fn ffi_nip65_config() -> NmpEngineConfig {
    NmpEngineConfig {
        nip65: Some(FfiNip65Config {
            indexer_relays: vec![PARITY_INDEXER.to_string()],
        }),
        ..NmpEngineConfig::default()
    }
}

#[test]
fn direct_and_public_ffi_nip22_comment_intents_are_exactly_identical() {
    let root_author = Keys::generate().public_key();
    let root_event_id = nostr::EventId::from_slice(&[0x11; 32]).unwrap();
    let parent_keys = Keys::generate();
    let content = "closed NIP-22 parity".to_string();
    let correlation = "nip22-parity-correlation";
    let relay = nostr::RelayUrl::parse("wss://parity.example").unwrap();

    // The parent is a real comment event on an addressable root, so BOTH
    // doors read the root scope off the same wire rows rather than either
    // side restating it. That is the property #1243's fold buys: the caller
    // cannot get the root wrong, on either surface.
    let parent = nostr::EventBuilder::new(Kind::from(1111u16), "parent comment")
        .tags([
            nostr::Tag::parse([
                "A".to_string(),
                format!("30023:{}:entry", root_author.to_hex()),
            ])
            .unwrap(),
            nostr::Tag::parse(["K".to_string(), "30023".to_string()]).unwrap(),
            nostr::Tag::parse(["P".to_string(), root_author.to_hex()]).unwrap(),
            nostr::Tag::parse(["E".to_string(), root_event_id.to_hex()]).unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from(1_700_000_000u64))
        .sign_with_keys(&parent_keys)
        .expect("parity fixture signs");
    let row = nmp::Row {
        event: parent.clone(),
        sources: std::collections::BTreeSet::from([relay.clone()]),
    };

    let direct = nmp_nip22::comment_intent(
        &row,
        content.clone(),
        Some(CorrelationToken::try_from(correlation).unwrap()),
    );
    let ffi = nmp_ffi::nip22::comment_intent(
        nmp_ffi::nip22::FfiCommentTarget::Row {
            row: nmp_ffi::convert::row_to_ffi_row(&row),
        },
        content,
        Some(correlation.to_string()),
    )
    .expect("the public FFI composer must accept the same closed inputs");

    let direct_builder = match &direct.payload {
        WritePayload::Event(builder) => builder,
        WritePayload::ReplaceableEdit { .. } | WritePayload::Signed(_) => {
            panic!("NIP-22 must compose one ordinary builder payload")
        }
    };
    let projected = nmp_ffi::convert::write_intent_from_ffi(ffi.clone())
        .expect("the public FFI result must be accepted by generic publish");
    let projected_builder = match &projected.payload {
        WritePayload::Event(builder) => builder,
        WritePayload::ReplaceableEdit { .. } | WritePayload::Signed(_) => {
            panic!("the public FFI result must stay an ordinary builder payload")
        }
    };
    assert_eq!(
        projected_builder, direct_builder,
        "direct Rust and public FFI must compose the identical builder"
    );
    assert_eq!(
        direct_builder.created_at, None,
        "neither door may invent a timestamp; acceptance stamps it"
    );

    // The addressable root revision stays pinned, the parent is the comment
    // event itself, and every pointer row carries the verified hint and the
    // author slot that the one door fills -- on both surfaces alike.
    let expected_tags = vec![
        vec![
            "A".to_string(),
            format!("30023:{}:entry", root_author.to_hex()),
            relay.to_string(),
            root_author.to_hex(),
        ],
        vec![
            "E".to_string(),
            root_event_id.to_hex(),
            relay.to_string(),
            root_author.to_hex(),
        ],
        vec!["K".to_string(), "30023".to_string()],
        vec!["P".to_string(), root_author.to_hex()],
        vec![
            "e".to_string(),
            parent.id.to_hex(),
            relay.to_string(),
            parent_keys.public_key().to_hex(),
        ],
        vec!["k".to_string(), "1111".to_string()],
        // The `p` row carries NO hint even though the `e` row does, and that
        // asymmetry is the honest one: an observed source is a verified fact
        // about where THAT EVENT is, and says nothing about where to find its
        // AUTHOR, which is an outbox fact. A caller who has resolved one
        // states it with `from_relay`.
        vec!["p".to_string(), parent_keys.public_key().to_hex()],
    ];
    assert_eq!(
        direct_builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>(),
        expected_tags,
        "the address root revision and direct parent identity must remain closed"
    );

    assert!(matches!(direct.routing, WriteRouting::Auto));
    assert_eq!(ffi.routing, FfiWriteRouting::Auto);
    assert_eq!(direct.identity, Identity::Active);
    assert_eq!(ffi.identity, FfiIdentity::Active);
    assert_eq!(
        direct.correlation.as_ref().map(ToString::to_string),
        Some(correlation.to_string())
    );
    assert_eq!(ffi.correlation.as_deref(), Some(correlation));
}

#[test]
fn retry_lane_receipt_truth_projects_exactly_from_direct_rust_to_ffi() {
    let relay = nostr::RelayUrl::parse("wss://receipt-parity.example").unwrap();
    let pubkey = nostr::Keys::generate().public_key();
    let awaited = nostr::Keys::generate().public_key();
    let expected_id = nostr::EventId::from_slice(&[0x5a; 32]).unwrap();
    let actual_id = nostr::EventId::from_slice(&[0x6b; 32]).unwrap();
    let cases = [
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::Waiting(RelayWaiting::NotConnected),
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::NotConnected,
                },
            },
        ),
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::Waiting(RelayWaiting::NeedsAuth),
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::NeedsAuth,
                },
            },
        ),
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::AuthFailed {
                    pubkey,
                    source: AuthDenialSource::Policy,
                    reason: "account not permitted".into(),
                },
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::AuthFailed {
                    pubkey: pubkey.to_hex(),
                    source: FfiAuthDenialSource::Policy,
                    reason: "account not permitted".into(),
                },
            },
        ),
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::Waiting(RelayWaiting::BackingOff {
                    attempt: 7,
                    eligible_at: Timestamp::from(123),
                    cause: RetryCause::RelayRateLimited,
                    detail: Some("rate-limited: slow down".into()),
                }),
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::BackingOff {
                        attempt: 7,
                        eligible_at: 123,
                        cause: FfiRetryCause::RelayRateLimited,
                        detail: Some("rate-limited: slow down".into()),
                    },
                },
            },
        ),
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                    detail: "attempt log stalled".into(),
                }),
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::PersistenceStalled {
                        detail: "attempt log stalled".into(),
                    },
                },
            },
        ),
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::Sent {
                    attempt: 9,
                    written_at: Timestamp::from(125),
                },
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::Sent {
                    attempt: 9,
                    written_at: 125,
                },
            },
        ),
        (
            WriteFact::Relay {
                relay: relay.clone(),
                state: RelayState::GaveUp,
            },
            FfiWriteFact::Relay {
                relay: relay.to_string(),
                state: FfiRelayState::GaveUp,
            },
        ),
        (
            WriteFact::Signing(SigningState::AwaitingSigner { pubkey }),
            FfiWriteFact::Signing {
                state: FfiSigningState::AwaitingSigner {
                    pubkey: pubkey.to_hex(),
                },
            },
        ),
        (
            // #1261: the two unsigned states are different facts, and the
            // boundary must not fold either onto the other.
            WriteFact::Signing(SigningState::InFlight { pubkey }),
            FfiWriteFact::Signing {
                state: FfiSigningState::InFlight {
                    pubkey: pubkey.to_hex(),
                },
            },
        ),
        (
            WriteFact::Signing(SigningState::Refused {
                reason: "signer said no".into(),
            }),
            FfiWriteFact::Signing {
                state: FfiSigningState::Refused {
                    reason: "signer said no".into(),
                },
            },
        ),
        (
            WriteFact::Destinations {
                relays: [relay.clone()].into_iter().collect(),
                complete: true,
                awaiting_author_routes: BTreeSet::new(),
            },
            FfiWriteFact::Destinations {
                relays: vec![relay.to_string()],
                complete: true,
                awaiting_author_routes: Vec::new(),
            },
        ),
        (
            // The park, which is the whole reason the field exists: an open
            // picture that NAMES who it waits on. A boundary that shipped
            // the emptiness and dropped the names would look identical to a
            // settled write from the tag alone.
            WriteFact::Destinations {
                relays: BTreeSet::new(),
                complete: false,
                awaiting_author_routes: [awaited].into_iter().collect(),
            },
            FfiWriteFact::Destinations {
                relays: Vec::new(),
                complete: false,
                awaiting_author_routes: vec![awaited.to_hex()],
            },
        ),
        (
            WriteFact::Outcome(WriteOutcome::Settled),
            FfiWriteFact::Outcome {
                outcome: FfiWriteOutcome::Settled,
            },
        ),
        (
            WriteFact::Outcome(WriteOutcome::NoDestination),
            FfiWriteFact::Outcome {
                outcome: FfiWriteOutcome::NoDestination,
            },
        ),
        (
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Superseded)),
            FfiWriteFact::Outcome {
                outcome: FfiWriteOutcome::NotSent {
                    reason: FfiNotSentReason::Superseded,
                },
            },
        ),
        (
            WriteFact::Outcome(WriteOutcome::Superseded),
            FfiWriteFact::Outcome {
                outcome: FfiWriteOutcome::Superseded,
            },
        ),
        // #1039: both event ids survive the boundary whole. Reduced to a
        // string this failure could only tell a user to redo the edit.
        (
            WriteFact::Outcome(WriteOutcome::Refused(
                RefuseReason::ReplaceableBaseChanged {
                    expected: Some(expected_id),
                    actual: Some(actual_id),
                },
            )),
            FfiWriteFact::Outcome {
                outcome: FfiWriteOutcome::Refused {
                    reason: FfiRefuseReason::ReplaceableBaseChanged {
                        expected: Some(expected_id.to_hex()),
                        actual: Some(actual_id.to_hex()),
                    },
                },
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

/// The `nmp::WriteFact` vocabulary flattened into one comparable value, with
/// every payload kept. The arms mirror the three axes the fact enum
/// separates — whole-write signing, one relay, and the whole-write terminal —
/// so a boundary that folded two of them together could not pass this oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NormStatus {
    /// #47 Unit B: carries the parked pubkey (hex) so the direct/FFI
    /// parity proof covers the payload, not just the variant tag.
    AwaitingSigner(String),
    /// #1261: a signer HAS the request. Carried separately from
    /// `AwaitingSigner` because a boundary that folded the two would tell an
    /// app every healthy write is parked on a key nobody has.
    SigningInFlight(String),
    Signed(String),
    SigningRefused(String),
    /// Both routing axes plus the reason the open one is open: the relays
    /// named so far, whether resolution can still grow, and the hex authors
    /// whose routes it is still waiting on. All three are payload, not tags,
    /// so a boundary that dropped any of them would pass a tag-only oracle —
    /// and the third is the one #1236 added, so it is carried here rather
    /// than normalized away.
    Destinations(Vec<String>, bool, Vec<String>),
    WaitingNotConnected(String),
    WaitingNeedsAuth(String),
    BackingOff(String, u64, u64, String, Option<String>),
    PersistenceStalled(String, String),
    Sent(String),
    Published(String),
    Rejected(String, String),
    AuthFailed(String, String, String, String),
    GaveUp(String),
    /// The destination set is closed and every relay in it is terminal.
    Settled,
    NoDestination,
    /// Terminal, and like `Settled` it carries nothing beyond WHICH not-sent
    /// reason it was: a retired obligation's whole content is that a newer
    /// write at the same NIP-01 address took its place.
    NotSent(&'static str),
    /// Replaced after local transport may have observed the bytes.
    Superseded,
    Refused(NormRefuseReason),
}

/// `nmp_store::RefuseReason` flattened. `ReplaceableBaseChanged` keeps BOTH
/// ids: that pair is what makes the failure recoverable without troubling a
/// user, so a boundary that dropped either half must fail here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NormRefuseReason {
    AlreadyExpired,
    Tombstoned,
    ReplaceableBaseOnRegularEvent,
    ReplaceableBaseChanged(Option<String>, Option<String>),
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
    /// (stage label, detail, stalled-since instant) per bounded detail row,
    /// in the order each surface delivered it -- the ORDER is part of the
    /// contract, so this is deliberately not sorted.
    stalled_writes: Vec<(String, String, u64)>,
    /// (unroutable, unsignable, undeliverable, omitted_details, detail_limit)
    stalled_write_totals: (u64, u64, u64, u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandoffBaseline {
    anchor: u64,
    content: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ScenarioOutcome {
    rows: Vec<NormRow>,
    /// Per-BRANCH evidence in canonical branch order (#1108).
    evidence: Vec<NormEvidence>,
    receipts: Vec<NormStatus>,
    diagnostics: NormDiagnostics,
}

#[derive(Debug, PartialEq, Eq)]
struct TamperedOutcome {
    /// #1237: a signature that does not verify is an instruction that cannot
    /// resolve, so `publish` refuses the CALL. There is no receipt and no
    /// queue entry — what both surfaces must agree on is the refusal itself,
    /// verbatim, and that the queue stayed empty.
    refusal: String,
    queue_len: usize,
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

// #680: bridge each pull-based FFI stream handle into an mpsc channel via a
// forwarding Tokio task, so every existing mpsc-based collect/wait helper is
// reused unchanged. The task holds a clone of the handle, keeping the stream
// open until the pull yields `None` (cancel / shutdown / producer drop). The
// caller keeps its own handle clone for explicit `cancel()`.
//
// #762: rows are claimed with a ticket before the await and committed only
// once this task actually holds the frame -- the same two-phase discipline the
// Swift and Kotlin wrappers use, so the direct/FFI oracle exercises it too.
fn bridge_rows(
    stream: &Arc<NmpRowStream>,
) -> mpsc::Receiver<(Vec<FfiRowDelta>, Vec<FfiAcquisitionEvidence>)> {
    let (tx, rx) = mpsc::channel();
    let stream = Arc::clone(stream);
    tokio::spawn(async move {
        loop {
            let Ok(pull) = stream.begin_next() else {
                break;
            };
            let Ok(Some(frame)) = pull.receive().await else {
                break;
            };
            if pull.commit().is_err() {
                break;
            }
            // Unbounded FFI observations carry deltas + evidence; `window` is
            // always `None` (windowing is a policy on the read noun, #485).
            if tx.send((frame.deltas, frame.evidence)).is_err() {
                break;
            }
        }
    });
    rx
}

fn bridge_diagnostics(
    stream: &Arc<NmpDiagnosticsStream>,
) -> mpsc::Receiver<FfiDiagnosticsSnapshot> {
    let (tx, rx) = mpsc::channel();
    let stream = Arc::clone(stream);
    tokio::spawn(async move {
        while let Ok(Some(snapshot)) = stream.next().await {
            if tx.send(snapshot).is_err() {
                break;
            }
        }
    });
    rx
}

fn bridge_receipts(stream: &Arc<NmpReceiptStream>) -> mpsc::Receiver<FfiWriteFact> {
    let (tx, rx) = mpsc::channel();
    let stream = Arc::clone(stream);
    tokio::spawn(async move {
        while let Ok(Some(status)) = stream.next().await {
            if tx.send(status).is_err() {
                break;
            }
        }
    });
    rx
}

fn bridge_follow_snapshots(stream: &Arc<NmpFollowStream>) -> mpsc::Receiver<FfiFollowSnapshot> {
    let (tx, rx) = mpsc::channel();
    let stream = Arc::clone(stream);
    tokio::spawn(async move {
        while let Ok(Some(snapshot)) = stream.next().await {
            if tx.send(snapshot).is_err() {
                break;
            }
        }
    });
    rx
}

fn bridge_follow_actions(
    stream: &Arc<NmpFollowActionStream>,
) -> mpsc::Receiver<FfiFollowActionStatus> {
    let (tx, rx) = mpsc::channel();
    let stream = Arc::clone(stream);
    tokio::spawn(async move {
        while let Ok(Some(status)) = stream.next().await {
            if tx.send(status).is_err() {
                break;
            }
        }
    });
    rx
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

// The direct-Rust receipt streams are `nmp::FifoReceiver`s; the FFI streams are
// bridged into `mpsc::Receiver`s. Both expose the same timed pull, so
// `recv_before` is generic over this one trait (#680: receipts became FIFO).
trait TimedRecv<T> {
    fn recv_before_timeout(&self, timeout: Duration) -> Result<T, FifoRecvTimeoutError>;
}

impl<T> TimedRecv<T> for mpsc::Receiver<T> {
    fn recv_before_timeout(&self, timeout: Duration) -> Result<T, FifoRecvTimeoutError> {
        self.recv_timeout(timeout).map_err(|error| match error {
            RecvTimeoutError::Timeout => FifoRecvTimeoutError::Timeout,
            RecvTimeoutError::Disconnected => FifoRecvTimeoutError::Closed,
        })
    }
}

impl<T> TimedRecv<T> for FifoReceiver<T> {
    fn recv_before_timeout(&self, timeout: Duration) -> Result<T, FifoRecvTimeoutError> {
        self.recv_timeout(timeout)
    }
}

fn recv_before<T, R: TimedRecv<T>>(rx: &R, deadline: Instant, what: &str) -> T {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
        !remaining.is_zero(),
        "{what} did not settle within the total {:?} bound",
        WAIT
    );
    rx.recv_before_timeout(remaining).unwrap_or_else(|error| {
        panic!("{what} did not settle within the total {WAIT:?} bound: {error:?}")
    })
}

fn lane_name(lane: Lane) -> &'static str {
    match lane {
        Lane::AuthorOutbound => "author_outbound",
        Lane::Hint => "hint",
        Lane::Provenance => "provenance",
        Lane::OperatorApp => "operator_app",
        Lane::OperatorFallback => "operator_fallback",
        Lane::Exact => "exact",
    }
}

fn direct_status_name(status: SourceStatus) -> String {
    match status {
        SourceStatus::Requesting => "requesting".to_string(),
        SourceStatus::FinishedStoredEvents => "finished_stored_events".to_string(),
        SourceStatus::AwaitingRequest => "awaiting_request".to_string(),
        SourceStatus::CoverageSatisfied => "coverage_satisfied".to_string(),
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
        FfiSourceStatus::FinishedStoredEvents => "finished_stored_events".to_string(),
        FfiSourceStatus::AwaitingRequest => "awaiting_request".to_string(),
        FfiSourceStatus::CoverageSatisfied => "coverage_satisfied".to_string(),
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

/// Normalize one observation's PER-BRANCH evidence (#1108) into the
/// order-insensitive shape the direct/FFI oracle compares. Branch order is
/// preserved: entry `i` on one side must equal entry `i` on the other.
fn normalize_direct_evidence(evidence: Vec<AcquisitionEvidence>, relay: &str) -> Vec<NormEvidence> {
    evidence
        .into_iter()
        .map(|branch| normalize_direct_branch_evidence(branch, relay))
        .collect()
}

fn normalize_ffi_evidence(evidence: Vec<FfiAcquisitionEvidence>, relay: &str) -> Vec<NormEvidence> {
    evidence
        .into_iter()
        .map(|branch| normalize_ffi_branch_evidence(branch, relay))
        .collect()
}

fn normalize_direct_branch_evidence(evidence: AcquisitionEvidence, relay: &str) -> NormEvidence {
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

fn normalize_ffi_branch_evidence(evidence: FfiAcquisitionEvidence, relay: &str) -> NormEvidence {
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

fn auth_denial_source_name(source: AuthDenialSource) -> &'static str {
    match source {
        AuthDenialSource::Policy => "policy",
        AuthDenialSource::Signer => "signer",
        AuthDenialSource::Relay => "relay",
    }
}

fn ffi_auth_denial_source_name(source: FfiAuthDenialSource) -> &'static str {
    match source {
        FfiAuthDenialSource::Policy => "policy",
        FfiAuthDenialSource::Signer => "signer",
        FfiAuthDenialSource::Relay => "relay",
    }
}

fn retry_cause_name(cause: RetryCause) -> &'static str {
    match cause {
        RetryCause::Interrupted => "interrupted",
        RetryCause::AckTimeout => "ack_timeout",
        RetryCause::ConnectionLost => "connection_lost",
        RetryCause::RelayRateLimited => "relay_rate_limited",
        RetryCause::RelayError => "relay_error",
    }
}

fn ffi_retry_cause_name(cause: FfiRetryCause) -> &'static str {
    match cause {
        FfiRetryCause::Interrupted => "interrupted",
        FfiRetryCause::AckTimeout => "ack_timeout",
        FfiRetryCause::ConnectionLost => "connection_lost",
        FfiRetryCause::RelayRateLimited => "relay_rate_limited",
        FfiRetryCause::RelayError => "relay_error",
    }
}

fn normalize_direct_refuse_reason(reason: RefuseReason) -> NormRefuseReason {
    match reason {
        RefuseReason::AlreadyExpired => NormRefuseReason::AlreadyExpired,
        RefuseReason::Tombstoned => NormRefuseReason::Tombstoned,
        RefuseReason::ReplaceableBaseOnRegularEvent => {
            NormRefuseReason::ReplaceableBaseOnRegularEvent
        }
        RefuseReason::ReplaceableBaseChanged { expected, actual } => {
            NormRefuseReason::ReplaceableBaseChanged(
                expected.map(|id| id.to_hex()),
                actual.map(|id| id.to_hex()),
            )
        }
    }
}

fn normalize_ffi_refuse_reason(reason: FfiRefuseReason) -> NormRefuseReason {
    match reason {
        FfiRefuseReason::AlreadyExpired => NormRefuseReason::AlreadyExpired,
        FfiRefuseReason::Tombstoned => NormRefuseReason::Tombstoned,
        FfiRefuseReason::ReplaceableBaseOnRegularEvent => {
            NormRefuseReason::ReplaceableBaseOnRegularEvent
        }
        FfiRefuseReason::ReplaceableBaseChanged { expected, actual } => {
            NormRefuseReason::ReplaceableBaseChanged(expected, actual)
        }
    }
}

fn normalize_direct_status(status: WriteFact, relay: &str) -> NormStatus {
    match status {
        WriteFact::Signing(SigningState::AwaitingSigner { pubkey }) => {
            NormStatus::AwaitingSigner(pubkey.to_hex())
        }
        WriteFact::Signing(SigningState::InFlight { pubkey }) => {
            NormStatus::SigningInFlight(pubkey.to_hex())
        }
        WriteFact::Signing(SigningState::Signed { event_id }) => {
            NormStatus::Signed(event_id.to_hex())
        }
        WriteFact::Signing(SigningState::Refused { reason }) => NormStatus::SigningRefused(reason),
        WriteFact::Destinations {
            relays,
            complete,
            awaiting_author_routes,
        } => NormStatus::Destinations(
            relays
                .iter()
                .map(|url| normalize_url(url.as_str(), relay))
                .collect(),
            complete,
            awaiting_author_routes
                .iter()
                .map(PublicKey::to_hex)
                .collect(),
        ),
        WriteFact::Relay { relay: url, state } => {
            let url = normalize_url(url.as_str(), relay);
            match state {
                RelayState::Waiting(RelayWaiting::NotConnected) => {
                    NormStatus::WaitingNotConnected(url)
                }
                RelayState::Waiting(RelayWaiting::NeedsAuth) => NormStatus::WaitingNeedsAuth(url),
                RelayState::Waiting(RelayWaiting::BackingOff {
                    attempt,
                    eligible_at,
                    cause,
                    detail,
                }) => NormStatus::BackingOff(
                    url,
                    attempt,
                    eligible_at.as_secs(),
                    retry_cause_name(cause).into(),
                    detail,
                ),
                RelayState::Waiting(RelayWaiting::PersistenceStalled { detail }) => {
                    NormStatus::PersistenceStalled(url, detail)
                }
                RelayState::Sent { .. } => NormStatus::Sent(url),
                RelayState::Published => NormStatus::Published(url),
                RelayState::Rejected { reason } => NormStatus::Rejected(url, reason),
                RelayState::AuthFailed {
                    pubkey,
                    source,
                    reason,
                } => NormStatus::AuthFailed(
                    url,
                    pubkey.to_hex(),
                    auth_denial_source_name(source).into(),
                    reason,
                ),
                RelayState::GaveUp => NormStatus::GaveUp(url),
            }
        }
        WriteFact::Outcome(WriteOutcome::Settled) => NormStatus::Settled,
        WriteFact::Outcome(WriteOutcome::NoDestination) => NormStatus::NoDestination,
        WriteFact::Outcome(WriteOutcome::NotSent(reason)) => {
            NormStatus::NotSent(not_sent_reason_name(reason))
        }
        WriteFact::Outcome(WriteOutcome::Superseded) => NormStatus::Superseded,
        WriteFact::Outcome(WriteOutcome::Refused(reason)) => {
            NormStatus::Refused(normalize_direct_refuse_reason(reason))
        }
    }
}

fn normalize_ffi_status(status: FfiWriteFact, relay: &str) -> NormStatus {
    match status {
        FfiWriteFact::Signing {
            state: FfiSigningState::AwaitingSigner { pubkey },
        } => NormStatus::AwaitingSigner(pubkey),
        FfiWriteFact::Signing {
            state: FfiSigningState::InFlight { pubkey },
        } => NormStatus::SigningInFlight(pubkey),
        FfiWriteFact::Signing {
            state: FfiSigningState::Signed { event_id },
        } => NormStatus::Signed(event_id),
        FfiWriteFact::Signing {
            state: FfiSigningState::Refused { reason },
        } => NormStatus::SigningRefused(reason),
        FfiWriteFact::Destinations {
            mut relays,
            complete,
            mut awaiting_author_routes,
        } => {
            for url in &mut relays {
                *url = normalize_url(url, relay);
            }
            relays.sort();
            awaiting_author_routes.sort();
            NormStatus::Destinations(relays, complete, awaiting_author_routes)
        }
        FfiWriteFact::Relay { relay: url, state } => {
            let url = normalize_url(&url, relay);
            match state {
                FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::NotConnected,
                } => NormStatus::WaitingNotConnected(url),
                FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::NeedsAuth,
                } => NormStatus::WaitingNeedsAuth(url),
                FfiRelayState::Waiting {
                    waiting:
                        FfiRelayWaiting::BackingOff {
                            attempt,
                            eligible_at,
                            cause,
                            detail,
                        },
                } => NormStatus::BackingOff(
                    url,
                    attempt,
                    eligible_at,
                    ffi_retry_cause_name(cause).into(),
                    detail,
                ),
                FfiRelayState::Waiting {
                    waiting: FfiRelayWaiting::PersistenceStalled { detail },
                } => NormStatus::PersistenceStalled(url, detail),
                FfiRelayState::Sent { .. } => NormStatus::Sent(url),
                FfiRelayState::Published => NormStatus::Published(url),
                FfiRelayState::Rejected { reason } => NormStatus::Rejected(url, reason),
                FfiRelayState::AuthFailed {
                    pubkey,
                    source,
                    reason,
                } => NormStatus::AuthFailed(
                    url,
                    pubkey,
                    ffi_auth_denial_source_name(source).into(),
                    reason,
                ),
                FfiRelayState::GaveUp => NormStatus::GaveUp(url),
            }
        }
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::Settled,
        } => NormStatus::Settled,
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::NoDestination,
        } => NormStatus::NoDestination,
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::NotSent { reason },
        } => NormStatus::NotSent(ffi_not_sent_reason_name(reason)),
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::Superseded,
        } => NormStatus::Superseded,
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::Refused { reason },
        } => NormStatus::Refused(normalize_ffi_refuse_reason(reason)),
    }
}

fn not_sent_reason_name(reason: NotSentReason) -> &'static str {
    match reason {
        NotSentReason::Cancelled => "cancelled",
        NotSentReason::SignerRefused => "signer-refused",
        NotSentReason::Superseded => "superseded",
    }
}

fn ffi_not_sent_reason_name(reason: FfiNotSentReason) -> &'static str {
    match reason {
        FfiNotSentReason::Cancelled => "cancelled",
        FfiNotSentReason::SignerRefused => "signer-refused",
        FfiNotSentReason::Superseded => "superseded",
    }
}

/// Whether this fact ends the WHOLE write. `WriteOutcome` is the only thing
/// that does — a relay terminal closes one lane, never the write.
fn is_whole_write_terminal(status: &NormStatus) -> bool {
    matches!(
        status,
        NormStatus::Settled
            | NormStatus::NoDestination
            | NormStatus::NotSent(_)
            | NormStatus::Superseded
            | NormStatus::Refused(_)
            | NormStatus::SigningRefused(_)
    )
}

/// Whether this fact ends ONE relay lane.
fn is_relay_terminal(status: &NormStatus) -> bool {
    matches!(
        status,
        NormStatus::Published(_)
            | NormStatus::Rejected(_, _)
            | NormStatus::AuthFailed(_, _, _, _)
            | NormStatus::GaveUp(_)
    )
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
    let totals = snapshot.stalled_write_totals;
    NormDiagnostics {
        relays,
        uncovered_author_count: snapshot.uncovered_author_count,
        dropped_merge_rules,
        stalled_writes: snapshot
            .stalled_writes
            .into_iter()
            .map(|write| {
                (
                    match write.stage {
                        StalledWriteStage::Unroutable => "unroutable".to_string(),
                        StalledWriteStage::Unsignable => "unsignable".to_string(),
                        StalledWriteStage::Undeliverable => "undeliverable".to_string(),
                    },
                    write.detail,
                    write.stalled_since.as_secs(),
                )
            })
            .collect(),
        stalled_write_totals: (
            totals.unroutable,
            totals.unsignable,
            totals.undeliverable,
            totals.omitted_details,
            totals.detail_limit,
        ),
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
    let totals = snapshot.stalled_write_totals;
    NormDiagnostics {
        relays,
        uncovered_author_count: snapshot.uncovered_author_count as usize,
        dropped_merge_rules,
        stalled_writes: snapshot
            .stalled_writes
            .into_iter()
            .map(|write| {
                (
                    match write.stage {
                        FfiStalledWriteStage::Unroutable => "unroutable".to_string(),
                        FfiStalledWriteStage::Unsignable => "unsignable".to_string(),
                        FfiStalledWriteStage::Undeliverable => "undeliverable".to_string(),
                    },
                    write.detail,
                    write.stalled_since,
                )
            })
            .collect(),
        stalled_write_totals: (
            totals.unroutable,
            totals.unsignable,
            totals.undeliverable,
            totals.omitted_details,
            totals.detail_limit,
        ),
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

fn parity_diagnostic_relays(
    snapshot: &NormDiagnostics,
) -> Option<(&NormRelayDiagnostics, &NormRelayDiagnostics)> {
    if snapshot.relays.len() != 2 {
        return None;
    }
    let content = snapshot
        .relays
        .iter()
        .find(|relay| relay.relay == "<loopback-relay>")?;
    let indexer = snapshot
        .relays
        .iter()
        .find(|relay| relay.relay == PARITY_INDEXER)?;
    Some((content, indexer))
}

fn handoff_is_quiescent(
    snapshot: &NormDiagnostics,
    relay_witness: &ScriptedRelay,
) -> Option<HandoffBaseline> {
    let (relay, indexer) = parity_diagnostic_relays(snapshot)?;
    let has_anchor = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, SOURCE_ANCHOR_KIND));
    let has_content = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, QUERY_KIND));
    let has_nip65_query = indexer
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, Kind::RelayList.as_u16()));
    let routed_through_app_policy = relay
        .by_lane
        .iter()
        .any(|(lane, count)| lane == "operator_app" && *count > 0);
    let baseline = HandoffBaseline {
        anchor: relay_witness.query_count_for_kind(SOURCE_ANCHOR_KIND),
        content: relay_witness.query_count_for_kind(QUERY_KIND),
    };
    // The explicit source-anchor subscription is still owned at this barrier,
    // so its filter must remain until the named cancellation below. Both
    // facades assemble the same NIP-65 provider and therefore expose the same
    // independent kind:10002 indexer request while ordinary reads route
    // through app policy.
    (has_anchor
        && has_content
        && has_nip65_query
        && routed_through_app_policy
        && baseline.anchor != 0
        && baseline.content != 0
        && event_count(relay, SOURCE_ANCHOR_KIND) == baseline.anchor
        && event_count(relay, QUERY_KIND) == baseline.content)
        .then_some(baseline)
}

fn content_phase_is_quiescent(
    snapshot: &NormDiagnostics,
    baseline: HandoffBaseline,
    relay_witness: &ScriptedRelay,
) -> bool {
    let Some((relay, indexer)) = parity_diagnostic_relays(snapshot) else {
        return false;
    };
    let has_content = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, QUERY_KIND));
    let has_stale_anchor = relay
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, SOURCE_ANCHOR_KIND));
    let has_nip65_query = indexer
        .filters
        .iter()
        .any(|filter| filter_names_kind(filter, Kind::RelayList.as_u16()));
    let routed_through_app_policy = relay
        .by_lane
        .iter()
        .any(|(lane, count)| lane == "operator_app" && *count > 0);
    let content_req_count = relay_witness.query_count_for_kind(QUERY_KIND);
    let anchor_req_count = relay_witness.query_count_for_kind(SOURCE_ANCHOR_KIND);
    has_content
        && !has_stale_anchor
        && has_nip65_query
        && routed_through_app_policy
        && content_req_count == baseline.content
        && anchor_req_count == baseline.anchor
        && event_count(relay, QUERY_KIND) == baseline.content
        && event_count(relay, SOURCE_ANCHOR_KIND) == baseline.anchor
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
        "{surface} diagnostics must contain only the app-policy-routed content plan, \
         with content/source-anchor REQs and events unchanged from the drained handoff \
         baseline {baseline:?}: {snapshot:?}"
    );
}

// Borrow the live source-anchor subscription across the barrier. This is an
// ownership witness, not data input: the caller cannot consume/cancel the
// subscription before the pre-cancel diagnostics state has been accepted.
fn wait_for_direct_handoff_quiescence(
    _anchor_subscription: &Subscription,
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
                 anchor={}, content={}",
                relay.query_count_for_kind(SOURCE_ANCHOR_KIND),
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
                 anchor={}, content={}",
                relay.query_count_for_kind(SOURCE_ANCHOR_KIND),
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

fn expected_limited_evidence() -> Vec<NormEvidence> {
    // One branch, so exactly one evidence entry (#1108).
    vec![NormEvidence {
        sources: vec![NormSource {
            relay: "<loopback-relay>".to_string(),
            reconciled_through: None,
            status: "requesting".to_string(),
        }],
        shortfall: vec![],
    }]
}

/// How far a receipt collector reads.
///
/// #1237 gave the write vocabulary a whole-write terminal
/// (`WriteOutcome`), and a write whose destination set CLOSES now ends on
/// it. A write whose destination set never closes — every `Auto` scenario in
/// this crate, where the identically assembled provider's indexer deliberately
/// never settles the author's route fact — has no such terminal, so collectors
/// stop at the relay lane's own terminal. Stating which one is expected
/// keeps both cases exact instead of trading a hang for a tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadUntil {
    /// The destination set closes, so `WriteOutcome` must arrive.
    WholeWriteTerminal,
    /// Routing stays open forever; the lane terminal is all there is.
    RelayTerminal,
}

fn collect_direct_receipts(
    rx: FifoReceiver<WriteFact>,
    relay: &str,
    until: ReadUntil,
) -> Vec<NormStatus> {
    let mut statuses = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let status = recv_before(&rx, deadline, "direct receipt");
        let normalized = normalize_direct_status(status, relay);
        let done = is_whole_write_terminal(&normalized)
            || (until == ReadUntil::RelayTerminal && is_relay_terminal(&normalized));
        statuses.push(normalized);
        if done {
            return statuses;
        }
    }
}

/// Bounded sibling of [`collect_direct_receipts`] for the fail-closed AUTH
/// park: there IS no terminal status (the lane parks), so collection stops
/// at the first `AwaitingAuth` beat instead. Borrows the receiver so the
/// caller can afterwards prove NO further status arrives.
fn collect_direct_receipts_until_awaiting_auth(
    rx: &FifoReceiver<WriteFact>,
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
        let parked = sent && matches!(normalized, NormStatus::WaitingNeedsAuth(_));
        statuses.push(normalized);
        if parked {
            return statuses;
        }
    }
}

fn collect_ffi_receipts_until_awaiting_auth(
    rx: &mpsc::Receiver<FfiWriteFact>,
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
        let parked = sent && matches!(normalized, NormStatus::WaitingNeedsAuth(_));
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
fn expected_send_preamble(keys: &Keys, route_complete: bool) -> Vec<NormStatus> {
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
        // #1237: acceptance is `publish` returning `Ok`, so no fact reports
        // it. The first thing the stream can say is that the write is signed.
        NormStatus::Signed(event.id.to_hex()),
        // These Auto scenarios get an executable destination from operator app
        // policy while the author's neutral route fact remains Unknown.
        // Delivery may finish; routing truthfully stays open because the
        // identically assembled provider's indexer never answers.
        // An open picture here is open for exactly one reason -- the
        // author's own neutral route fact is Unknown -- so the park names
        // that one key and a closed picture names nobody.
        NormStatus::Destinations(
            vec![relay.clone()],
            route_complete,
            if route_complete {
                Vec::new()
            } else {
                vec![keys.public_key().to_hex()]
            },
        ),
        NormStatus::WaitingNotConnected(relay.clone()),
        NormStatus::WaitingNeedsAuth(relay.clone()),
        NormStatus::Sent(relay),
    ]
}

/// Every fact a successful write exposes. When the destination set CLOSES
/// (`route_complete`), the write also reaches its whole-write terminal:
/// `Settled` follows the last relay terminal and nothing may follow it.
fn expected_success_receipts(keys: &Keys, route_complete: bool) -> Vec<NormStatus> {
    let mut receipts = expected_send_preamble(keys, route_complete);
    receipts.push(NormStatus::Published("<loopback-relay>".to_string()));
    if route_complete {
        receipts.push(NormStatus::Settled);
    }
    receipts
}

/// #8 U2 fail-closed park: no AUTH policy registry exists at this wave, so
/// against a relay that answers an unauthenticated EVENT with
/// `OK false "auth-required:"` the write emits exactly one `AwaitingAuth`
/// beat and the lane stays parked — no retry, no terminal status.
fn expected_auth_parked_receipts(keys: &Keys) -> Vec<NormStatus> {
    let mut receipts = expected_send_preamble(keys, false);
    receipts.push(NormStatus::WaitingNeedsAuth("<loopback-relay>".to_string()));
    receipts
}

fn stage_direct_source_anchor(
    engine: &Engine,
    pubkey: &str,
    relay: &ScriptedRelay,
) -> Subscription {
    let subscription = engine
        .observe(
            LiveQuery::from_filter(direct_filter(pubkey, SOURCE_ANCHOR_KIND)),
            None,
        )
        .expect("direct source-anchor query must open");

    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = subscription
            .recv_timeout(remaining)
            .unwrap_or_else(|error| {
                panic!(
                    "direct source-anchor query did not settle within the total {WAIT:?} bound: {error}"
                )
            });
        let evidence = normalize_direct_evidence(frame.evidence, relay.url.as_str());
        if evidence == expected_limited_evidence() {
            break;
        }
    }
    subscription
}

fn stage_ffi_source_anchor(
    engine: &NmpEngine,
    pubkey: &str,
    relay: &ScriptedRelay,
) -> Arc<NmpRowStream> {
    let handle = engine
        .observe(ffi_filter(pubkey, SOURCE_ANCHOR_KIND), None)
        .expect("FFI source-anchor query must open");
    let rx = bridge_rows(&handle);

    let deadline = Instant::now() + WAIT;
    loop {
        let (_deltas, evidence) = recv_before(&rx, deadline, "FFI source-anchor query");
        let evidence = normalize_ffi_evidence(evidence, relay.url.as_str());
        if evidence == expected_limited_evidence() {
            break;
        }
    }
    handle
}

async fn setup_relay(keys: &Keys, query_event: &nostr::Event) -> ScriptedRelay {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let anchor = nostr::EventBuilder::new(Kind::Custom(SOURCE_ANCHOR_KIND), "source anchor")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT - 1))
        .sign_with_keys(keys)
        .expect("source anchor fixture must sign");
    relay.seed_signed_event(&anchor).await;
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

fn direct_follow_receipt_name(status: &WriteFact) -> &'static str {
    match status {
        WriteFact::Signing(SigningState::AwaitingSigner { .. }) => "awaiting_signer",
        WriteFact::Signing(SigningState::InFlight { .. }) => "signing_in_flight",
        WriteFact::Signing(SigningState::Signed { .. }) => "signed",
        WriteFact::Signing(SigningState::Refused { .. }) => "signing_refused",
        // `complete` is the routing AXIS's own terminal, and it is the only
        // thing that distinguishes "still discovering destinations" from
        // "this answer can never change again" (`resolution-lifecycle.md`
        // §7.2.1). Collapsing both into one word would let a retirement that
        // never happens compare equal to one that did.
        WriteFact::Destinations { complete: true, .. } => "routed_complete",
        WriteFact::Destinations { .. } => "routed",
        WriteFact::Relay { state, .. } => direct_relay_state_name(state),
        WriteFact::Outcome(WriteOutcome::Settled) => "settled",
        WriteFact::Outcome(WriteOutcome::NoDestination) => "no_destination",
        // A follow list is kind:3 — replaceable — so a second `set_following`
        // while the first is still unsent retires the first at the same
        // `(pubkey, kind)` address. Both surfaces must name that terminal
        // with the same word, and both must stop reading the stream on it.
        WriteFact::Outcome(WriteOutcome::NotSent(reason)) => not_sent_reason_name(*reason),
        WriteFact::Outcome(WriteOutcome::Superseded) => "superseded_after_handoff",
        WriteFact::Outcome(WriteOutcome::Refused(_)) => "refused",
    }
}

fn direct_relay_state_name(state: &RelayState) -> &'static str {
    match state {
        RelayState::Waiting(RelayWaiting::NotConnected) => "awaiting_relay",
        RelayState::Waiting(RelayWaiting::NeedsAuth) => "awaiting_auth",
        RelayState::Waiting(RelayWaiting::BackingOff { .. }) => "backing_off",
        RelayState::Waiting(RelayWaiting::PersistenceStalled { .. }) => "persistence_stalled",
        RelayState::Sent { .. } => "sent",
        RelayState::Published => "published",
        RelayState::Rejected { .. } => "rejected",
        RelayState::AuthFailed { .. } => "auth_failed",
        RelayState::GaveUp => "gave_up",
    }
}

fn ffi_follow_receipt_name(status: &FfiWriteFact) -> &'static str {
    match status {
        FfiWriteFact::Signing {
            state: FfiSigningState::AwaitingSigner { .. },
        } => "awaiting_signer",
        FfiWriteFact::Signing {
            state: FfiSigningState::InFlight { .. },
        } => "signing_in_flight",
        FfiWriteFact::Signing {
            state: FfiSigningState::Signed { .. },
        } => "signed",
        FfiWriteFact::Signing {
            state: FfiSigningState::Refused { .. },
        } => "signing_refused",
        FfiWriteFact::Destinations { complete: true, .. } => "routed_complete",
        FfiWriteFact::Destinations { .. } => "routed",
        FfiWriteFact::Relay { state, .. } => ffi_relay_state_name(state),
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::Settled,
        } => "settled",
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::NoDestination,
        } => "no_destination",
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::NotSent { reason },
        } => ffi_not_sent_reason_name(*reason),
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::Superseded,
        } => "superseded_after_handoff",
        FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::Refused { .. },
        } => "refused",
    }
}

fn ffi_relay_state_name(state: &FfiRelayState) -> &'static str {
    match state {
        FfiRelayState::Waiting {
            waiting: FfiRelayWaiting::NotConnected,
        } => "awaiting_relay",
        FfiRelayState::Waiting {
            waiting: FfiRelayWaiting::NeedsAuth,
        } => "awaiting_auth",
        FfiRelayState::Waiting {
            waiting: FfiRelayWaiting::BackingOff { .. },
        } => "backing_off",
        FfiRelayState::Waiting {
            waiting: FfiRelayWaiting::PersistenceStalled { .. },
        } => "persistence_stalled",
        FfiRelayState::Sent { .. } => "sent",
        FfiRelayState::Published => "published",
        FfiRelayState::Rejected { .. } => "rejected",
        FfiRelayState::AuthFailed { .. } => "auth_failed",
        FfiRelayState::GaveUp => "gave_up",
    }
}

/// Which axis of a follow action has and has not reached its own terminal.
///
/// Only used to make a bounded wait's expiry SAY something. A follow action
/// that never closes has stalled on exactly one of two independent axes, and
/// "timed out" alone sends the next reader down the entire instrumentation
/// path this rule was found on (`resolution-lifecycle.md` §7.2.1).
fn stalled_axes(seen: &[NormFollowActionStatus]) -> String {
    let routing = if seen
        .iter()
        .any(|s| matches!(s, NormFollowActionStatus::Receipt("routed_complete")))
    {
        "routing: RETIRED"
    } else if seen
        .iter()
        .any(|s| matches!(s, NormFollowActionStatus::Receipt("routed")))
    {
        "routing: STALLED (routed, never complete -- relay-list absence never settled)"
    } else {
        "routing: STALLED (never routed at all)"
    };
    let delivery = if seen.iter().any(|s| {
        matches!(
            s,
            NormFollowActionStatus::Receipt(
                "published"
                    | "rejected"
                    | "auth_failed"
                    | "gave_up"
                    | "settled"
                    | "no_destination"
                    | "cancelled"
                    | "superseded"
                    | "refused"
                    | "signing_refused"
            )
        )
    }) {
        "delivery: TERMINAL"
    } else {
        "delivery: STALLED"
    };
    format!("{routing}; {delivery}; seen={seen:?}")
}

/// This action's DELIVERY facts, in order, with the routing axis removed.
fn delivery_axis(seen: &[NormFollowActionStatus]) -> Vec<&'static str> {
    seen.iter()
        .filter_map(|status| match status {
            NormFollowActionStatus::Receipt("routed" | "routed_complete") => None,
            NormFollowActionStatus::Receipt(name) => Some(*name),
            _ => None,
        })
        .collect()
}

/// This action's ROUTING facts, in order. A write with unknowns emits
/// `routed` at least once and must end on `routed_complete`.
fn routing_axis(seen: &[NormFollowActionStatus]) -> Vec<&'static str> {
    seen.iter()
        .filter_map(|status| match status {
            NormFollowActionStatus::Receipt(name @ ("routed" | "routed_complete")) => Some(*name),
            _ => None,
        })
        .collect()
}

/// Re-express a drained follow action as its two axes, each in its own order,
/// routing last.
///
/// Closure makes the CONTENT of the stream stable — both axes have reached a
/// terminal by then — but it does not make the INTERLEAVING stable, and
/// measurement says so plainly: over twelve runs of this identical scenario
/// the routing retirement landed before `awaiting_auth` on one surface and
/// after it on the other, half the time, purely by which socket answered
/// first. Comparing the raw order therefore compares a race and fails about
/// every other run for no reason a reader can act on.
///
/// This is NOT the oracle tolerating a difference. Every fact is still
/// compared, and each axis's own order is still compared exactly; what is
/// dropped is the one degree of freedom `resolution-lifecycle.md` §7.2.1
/// already pins as unordered by construction — routing advances on provider
/// fact changes, delivery on delivery round-trips, and nothing sequences those
/// two against each other on either surface.
fn canonical_axes(seen: Vec<NormFollowActionStatus>) -> Vec<NormFollowActionStatus> {
    let (routing, rest): (Vec<_>, Vec<_>) = seen.into_iter().partition(|status| {
        matches!(
            status,
            NormFollowActionStatus::Receipt("routed" | "routed_complete")
        )
    });
    rest.into_iter().chain(routing).collect()
}

/// Drain through the action's delivery terminal.
///
/// Operator app policy supplies an executable route, while the identically
/// assembled NIP-65 provider waits on a deliberately nonanswering indexer.
/// The author's neutral route fact therefore remains open after delivery.
/// Waiting for stream closure here would falsely require settlement the test
/// environment never supplies.
fn collect_direct_follow_action(action: FollowAction) -> Vec<NormFollowActionStatus> {
    let deadline = Instant::now() + WAIT;
    let mut result = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let status = match action.recv_timeout(remaining) {
            Ok(status) => status,
            Err(FifoRecvTimeoutError::Closed) => return canonical_axes(result),
            Err(error) => panic!(
                "direct follow action did not close within the total {WAIT:?} bound \
                 ({error:?}) -- {}",
                stalled_axes(&result)
            ),
        };
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
        let done = matches!(
            normalized,
            NormFollowActionStatus::NoChange(_)
                | NormFollowActionStatus::Failed(_)
                | NormFollowActionStatus::Receipt(
                    "published"
                        | "rejected"
                        | "auth_failed"
                        | "gave_up"
                        | "settled"
                        | "no_destination"
                        | "cancelled"
                        | "superseded"
                        | "refused"
                        | "signing_refused"
                )
        );
        result.push(normalized);
        if done {
            return canonical_axes(result);
        }
    }
}

fn collect_ffi_follow_action(
    rx: &mpsc::Receiver<FfiFollowActionStatus>,
) -> Vec<NormFollowActionStatus> {
    let deadline = Instant::now() + WAIT;
    let mut result = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let status = match rx.recv_before_timeout(remaining) {
            Ok(status) => status,
            Err(FifoRecvTimeoutError::Closed) => return canonical_axes(result),
            Err(error) => panic!(
                "FFI follow action did not close within the total {WAIT:?} bound \
                 ({error:?}) -- {}",
                stalled_axes(&result)
            ),
        };
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
        let done = matches!(
            normalized,
            NormFollowActionStatus::NoChange(_)
                | NormFollowActionStatus::Failed(_)
                | NormFollowActionStatus::Receipt(
                    "published"
                        | "rejected"
                        | "auth_failed"
                        | "gave_up"
                        | "settled"
                        | "no_destination"
                        | "cancelled"
                        | "superseded"
                        | "refused"
                        | "signing_refused"
                )
        );
        result.push(normalized);
        if done {
            return canonical_axes(result);
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
            app_relays: vec![relay.url.to_string()],
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..direct_nip65_config()
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
        app_relays: vec![relay.url.to_string()],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..ffi_nip65_config()
    })
    .expect("FFI follow engine must construct");
    let active = engine
        .add_account(author.secret_key().to_secret_hex())
        .expect("FFI follow account must register");
    engine
        .set_active_account(Some(active.public_key()))
        .expect("FFI follow account must activate");

    let observation = engine
        .observe_following(target.public_key().to_hex())
        .expect("FFI following observation must open");
    let snapshot_rx = bridge_follow_snapshots(&observation);
    let initial = wait_for_ffi_follow_snapshot(&snapshot_rx, FfiFollowRelationship::NotFollowing);

    let follow_action = engine
        .follow(target.public_key().to_hex())
        .expect("FFI follow action must start with a configured route provider");
    let follow_rx = bridge_follow_actions(&follow_action);
    let follow = collect_ffi_follow_action(&follow_rx);
    let after_follow = wait_for_ffi_follow_snapshot(&snapshot_rx, FfiFollowRelationship::Following);

    let no_change_action = engine
        .follow(target.public_key().to_hex())
        .expect("FFI no-change action must start with a configured route provider");
    let no_change_rx = bridge_follow_actions(&no_change_action);
    let no_change = collect_ffi_follow_action(&no_change_rx);

    let unfollow_action = engine
        .unfollow(target.public_key().to_hex())
        .expect("FFI unfollow action must start with a configured route provider");
    let unfollow_rx = bridge_follow_actions(&unfollow_action);
    let unfollow = collect_ffi_follow_action(&unfollow_rx);
    let after_unfollow =
        wait_for_ffi_follow_snapshot(&snapshot_rx, FfiFollowRelationship::NotFollowing);

    let existing_observation = engine
        .observe_following(existing.public_key().to_hex())
        .expect("FFI preserved-follow observation must open");
    let existing_rx = bridge_follow_snapshots(&existing_observation);
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
    let engine = Arc::new(
        Engine::new(EngineConfig {
            app_relays: vec![relay.url.to_string()],
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..direct_nip65_config()
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
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        app_relays: vec![relay.url.to_string()],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..ffi_nip65_config()
    })
    .expect("FFI missing-list engine must construct");
    let active = engine
        .add_account(author.secret_key().to_secret_hex())
        .expect("FFI missing-list account must register");
    engine
        .set_active_account(Some(active.public_key()))
        .expect("FFI missing-list account must activate");

    let observation = engine
        .observe_following(target.public_key().to_hex())
        .expect("FFI missing-list observation must open");
    let snapshot_rx = bridge_follow_snapshots(&observation);
    let snapshot =
        wait_for_ffi_follow_availability(&snapshot_rx, FfiFollowAvailability::NoContactList);
    let action_handle = engine
        .follow(target.public_key().to_hex())
        .expect("FFI follow action must start with a configured route provider");
    let action_rx = bridge_follow_actions(&action_handle);
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
        app_relays: vec![relay_url.clone()],
        // Both facades assemble the same optional provider and app policy.
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..direct_nip65_config()
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

    let anchor_subscription = stage_direct_source_anchor(&engine, &pubkey.to_hex(), &relay);

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
    // demand reaches zero. Keep both observations live until actual relay
    // counters and cumulative diagnostics prove every admitted
    // source-anchor/content response crossed the handoff. That equality barrier
    // is the stable baseline; only then may withdrawing the anchor prove the
    // handoff caused no replay.
    let handoff_baseline =
        wait_for_direct_handoff_quiescence(&anchor_subscription, &diag_rx, &relay);
    anchor_subscription.cancel();

    let diagnostics_deadline = Instant::now() + WAIT;
    let mut last_diagnostics = None;
    let diagnostics = loop {
        let remaining = diagnostics_deadline.saturating_duration_since(Instant::now());
        let snapshot = diag_rx.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "direct diagnostics did not settle within the total {WAIT:?} bound: {error}; \
                 handoff baseline: {handoff_baseline:?}; last snapshot: {last_diagnostics:?}; \
                 relay query counts: anchor={}, content={}",
                relay.query_count_for_kind(SOURCE_ANCHOR_KIND),
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
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("direct publish must enqueue")
        .statuses;
    let receipts = collect_direct_receipts(receipt_rx, &relay_url, ReadUntil::RelayTerminal);
    assert_eq!(
        receipts,
        expected_success_receipts(keys, false),
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

fn collect_ffi_receipts(
    rx: &mpsc::Receiver<FfiWriteFact>,
    relay: &str,
    until: ReadUntil,
) -> Vec<NormStatus> {
    let mut statuses = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let status = recv_before(rx, deadline, "FFI receipt");
        let normalized = normalize_ffi_status(status, relay);
        let done = is_whole_write_terminal(&normalized)
            || (until == ReadUntil::RelayTerminal && is_relay_terminal(&normalized));
        statuses.push(normalized);
        if done {
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
        app_relays: vec![relay_url.clone()],
        fallback_relays: vec![],
        // Same provider and operator policy as `run_direct_success`.
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..ffi_nip65_config()
    })
    .expect("FFI engine must construct");
    let registration = engine
        .add_account(keys.secret_key().to_secret_hex())
        .expect("FFI account must register");
    let pubkey = registration.public_key();
    engine
        .set_active_account(Some(pubkey.clone()))
        .expect("FFI account must activate");

    let diagnostics_handle = engine
        .observe_diagnostics()
        .expect("FFI diagnostics must open");
    let diag_rx = bridge_diagnostics(&diagnostics_handle);
    let anchor_handle = stage_ffi_source_anchor(&engine, &pubkey, &relay);
    let query_handle = engine
        .observe(ffi_filter(&pubkey, QUERY_KIND), None)
        .expect("FFI query must open");
    let rows_rx = bridge_rows(&query_handle);
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
    // Same durable, continuously-owned handoff proof as the direct facade.
    let handoff_baseline = wait_for_ffi_handoff_quiescence(&diag_rx, &relay);
    anchor_handle.cancel();

    let diagnostics_deadline = Instant::now() + WAIT;
    let mut last_diagnostics = None;
    let diagnostics = loop {
        let remaining = diagnostics_deadline.saturating_duration_since(Instant::now());
        let snapshot = diag_rx.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "FFI diagnostics did not settle within the total {WAIT:?} bound: {error}; \
                 handoff baseline: {handoff_baseline:?}; last snapshot: {last_diagnostics:?}; \
                 relay query counts: anchor={}, content={}",
                relay.query_count_for_kind(SOURCE_ANCHOR_KIND),
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

    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                    created_at: Some(WRITE_CREATED_AT),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("FFI publish must enqueue");
    let receipt_rx = bridge_receipts(&receipt);
    let receipts = collect_ffi_receipts(&receipt_rx, &relay_url, ReadUntil::RelayTerminal);
    assert_eq!(
        receipts,
        expected_success_receipts(keys, false),
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

/// #8 U2 fail-closed AUTH park, direct half. Same provider/operator-source
/// preamble as `run_direct_success`, but the relay answers the
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
    let anchor = nostr::EventBuilder::new(Kind::Custom(SOURCE_ANCHOR_KIND), "source anchor")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT - 1))
        .sign_with_keys(keys)
        .expect("source anchor fixture must sign");
    relay.seed_signed_event(&anchor).await;
    relay.seed_signed_event(query_event).await;
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay_url.clone()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..direct_nip65_config()
    })
    .expect("direct auth-parked engine must construct");
    let pubkey = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("direct auth-parked account must register")
        .public_key();
    engine
        .set_active_account(Some(pubkey))
        .expect("direct auth-parked account must activate");

    let anchor_cancel = stage_direct_source_anchor(&engine, &pubkey.to_hex(), &relay);

    let unsigned = UnsignedEvent::new(
        pubkey,
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    );
    let receipt_rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("direct auth-parked publish must enqueue")
        .statuses;
    let receipts = collect_direct_receipts_until_awaiting_auth(&receipt_rx, &relay_url);
    assert_eq!(
        receipt_rx.recv_timeout(Duration::from_secs(2)),
        Err(FifoRecvTimeoutError::Timeout),
        "a fail-closed AUTH park must emit no further direct status: no retry, no terminal"
    );

    anchor_cancel.cancel();
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
    let anchor = nostr::EventBuilder::new(Kind::Custom(SOURCE_ANCHOR_KIND), "source anchor")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT - 1))
        .sign_with_keys(keys)
        .expect("source anchor fixture must sign");
    relay.seed_signed_event(&anchor).await;
    relay.seed_signed_event(query_event).await;
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        app_relays: vec![relay_url.clone()],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..ffi_nip65_config()
    })
    .expect("FFI auth-parked engine must construct");
    let registration = engine
        .add_account(keys.secret_key().to_secret_hex())
        .expect("FFI auth-parked account must register");
    let pubkey = registration.public_key();
    engine
        .set_active_account(Some(pubkey.clone()))
        .expect("FFI auth-parked account must activate");

    let anchor_handle = stage_ffi_source_anchor(&engine, &pubkey, &relay);

    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                    created_at: Some(WRITE_CREATED_AT),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("FFI auth-parked publish must enqueue");
    let receipt_rx = bridge_receipts(&receipt);
    let receipts = collect_ffi_receipts_until_awaiting_auth(&receipt_rx, &relay_url);
    assert_eq!(
        receipt_rx.recv_timeout(Duration::from_secs(2)),
        Err(RecvTimeoutError::Timeout),
        "a fail-closed AUTH park must emit no further FFI status: no retry, no terminal"
    );

    anchor_handle.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

/// #47 explicit-identity publish, direct half. The named pubkey is
/// registered as a SECONDARY account -- in the engine's signer set but
/// never active -- while the active account is a different registered
/// identity. The operator app source is identity-neutral: `Auto` still
/// freezes the NAMED identity as author before routing. A silent
/// fallback to the active account would sign a DIFFERENT author and change
/// the deterministic event id the `Signed` receipt names.
async fn run_direct_override_publish(active: &Keys, override_keys: &Keys) -> Vec<NormStatus> {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let anchor = nostr::EventBuilder::new(Kind::Custom(SOURCE_ANCHOR_KIND), "source anchor")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT - 1))
        .sign_with_keys(override_keys)
        .expect("source anchor fixture must sign");
    relay.seed_signed_event(&anchor).await;
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay_url.clone()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..direct_nip65_config()
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

    let anchor_cancel = stage_direct_source_anchor(&engine, &override_pubkey.to_hex(), &relay);

    let unsigned = UnsignedEvent::new(
        override_pubkey,
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    );
    let receipt_rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(override_pubkey),
            correlation: None,
        })
        .expect("direct override publish must enqueue")
        .statuses;
    let receipts = collect_direct_receipts(receipt_rx, &relay_url, ReadUntil::RelayTerminal);

    anchor_cancel.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

/// FFI half of the explicit-identity publish -- its own isolated relay instance and
/// the identical two-account construction as the direct half (active
/// account registered AND active, override registered but never active),
/// so the byte-identical receipt comparison is honest.
async fn run_ffi_override_publish(active: &Keys, override_keys: &Keys) -> Vec<NormStatus> {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let anchor = nostr::EventBuilder::new(Kind::Custom(SOURCE_ANCHOR_KIND), "source anchor")
        .custom_created_at(Timestamp::from(QUERY_CREATED_AT - 1))
        .sign_with_keys(override_keys)
        .expect("source anchor fixture must sign");
    relay.seed_signed_event(&anchor).await;
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        app_relays: vec![relay_url.clone()],
        fallback_relays: vec![],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..ffi_nip65_config()
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

    let anchor_handle = stage_ffi_source_anchor(&engine, &override_pubkey, &relay);

    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                    created_at: Some(WRITE_CREATED_AT),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Explicit {
                pubkey: override_pubkey,
            },
            correlation: None,
        })
        .expect("FFI override publish must enqueue");
    let receipt_rx = bridge_receipts(&receipt);
    let receipts = collect_ffi_receipts(&receipt_rx, &relay_url, ReadUntil::RelayTerminal);

    anchor_handle.cancel();
    engine.shutdown();
    relay.shutdown();
    receipts
}

async fn run_direct_tampered(keys: &Keys) -> TamperedOutcome {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay_url.clone()],
        ..direct_nip65_config()
    })
    .expect("direct tampered engine must construct");
    let mut event = nostr::EventBuilder::new(Kind::Custom(WRITE_KIND), "original")
        .custom_created_at(Timestamp::from(WRITE_CREATED_AT))
        .sign_with_keys(keys)
        .expect("tampered fixture must first sign cleanly");
    event.content = "tampered".to_string();
    let refusal = match engine.publish(WriteIntent {
        payload: WritePayload::Signed(event),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }) {
        Ok(_) => panic!("a tampered Signed payload must refuse the direct call itself"),
        Err(EngineError::PublishRefused { reason }) => reason,
        Err(other) => panic!("expected EngineError::PublishRefused, got {other:?}"),
    };
    let queue_len = engine
        .publish_queue()
        .expect("the direct engine is open")
        .len();
    engine.shutdown();
    let relay_contact_count = relay.contact_count();
    relay.shutdown();
    TamperedOutcome {
        refusal,
        queue_len,
        relay_contact_count,
    }
}

async fn run_ffi_tampered(keys: &Keys) -> TamperedOutcome {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        app_relays: vec![relay_url.clone()],
        fallback_relays: vec![],
        ..ffi_nip65_config()
    })
    .expect("FFI tampered engine must construct");
    let event = nostr::EventBuilder::new(Kind::Custom(WRITE_KIND), "original")
        .custom_created_at(Timestamp::from(WRITE_CREATED_AT))
        .sign_with_keys(keys)
        .expect("tampered fixture must first sign cleanly");
    let refusal = match engine.publish(FfiWriteIntent {
        payload: FfiWritePayload::Signed {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags: event.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
            content: "tampered".to_string(),
            sig: event.sig.to_string(),
        },
        routing: FfiWriteRouting::Auto,
        identity: FfiIdentity::Active,
        correlation: None,
    }) {
        Ok(_) => panic!("a tampered Signed payload must refuse the FFI call itself"),
        Err(FfiError::PublishRefused { reason }) => reason,
        Err(other) => panic!("expected FfiError::PublishRefused, got {other:?}"),
    };
    let queue_len = engine
        .publish_queue()
        .expect("the FFI engine is open")
        .len();
    engine.shutdown();
    let relay_contact_count = relay.contact_count();
    relay.shutdown();
    TamperedOutcome {
        refusal,
        queue_len,
        relay_contact_count,
    }
}

// #99: PR #97's FFI reattach coverage stopped at a pure enum-mapping unit
// test -- structural code-sharing (`nmp-ffi` delegates to the same
// `nmp::Engine`) is not itself proof, exactly the discipline this whole
// harness exists to enforce (module doc). The two scenarios below drive
// `reattach_receipt` through BOTH entry points and assert identical
// outcomes AND identical replayed fact sequences: one for a LIVE retained
// receipt (`Attached`, replaying `Signing(AwaitingSigner)`), one for a
// genuinely TERMINAL retained receipt reached by supersession at one
// replaceable coordinate (`Attached`, replaying the terminal
// `Outcome(NotSent(Superseded))`). Neither needs a relay at all -- the
// signer park and the supersession are purely local acceptance/persistence
// facts, independent of wire delivery.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormReattach {
    Attached,
    NotFound,
    RetainedButUnreadable,
}

fn direct_reattach_outcome(value: &ReceiptReattachment) -> NormReattach {
    match value {
        ReceiptReattachment::Attached { .. } => NormReattach::Attached,
        ReceiptReattachment::NotFound => NormReattach::NotFound,
        ReceiptReattachment::RetainedButUnreadable => NormReattach::RetainedButUnreadable,
    }
}

fn ffi_reattach_outcome(value: &FfiReceiptReattachment) -> NormReattach {
    match value {
        // #680: `Attached` now carries the pull-based receipt stream, so it is a
        // struct variant classified by ref (we drain the stream separately).
        FfiReceiptReattachment::Attached { .. } => NormReattach::Attached,
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
/// is ACTIVE but has no registered signer (so it parks in a genuinely
/// retained `Signing(AwaitingSigner)` steady state, never resolving
/// further), then reattach with a second, independent observer and prove it
/// replays the identical fact sequence the original saw.
async fn run_direct_reattach_live() -> ReattachProof {
    // Must match `run_ffi_reattach_live`'s identity: since #47 Unit B the
    // replayed `AwaitingSigner` carries the frozen author pubkey, so the
    // direct-vs-FFI `ReattachProof` equality now compares that hex payload.
    // A per-run `Keys::generate()` would make the two halves disagree by
    // construction; a shared fixed key makes the payload-parity real.
    let keys = fixed_keys();
    let engine = Engine::new(direct_nip65_config()).expect("direct engine must construct");
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
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("direct publish must enqueue");

    // #1237: acceptance is `publish` returning `Ok`, not a fact. The
    // park is the whole retained prefix.
    let deadline = Instant::now() + WAIT;
    assert_eq!(
        recv_before(
            &tracked.statuses,
            deadline,
            "direct original AwaitingSigner"
        ),
        WriteFact::Signing(SigningState::AwaitingSigner {
            pubkey: keys.public_key()
        })
    );

    let outcome = engine
        .reattach_receipt(tracked.id)
        .expect("direct reattach call must succeed while the engine is open");
    let norm_outcome = direct_reattach_outcome(&outcome);
    let replay = match outcome {
        ReceiptReattachment::Attached { statuses: rx, .. } => {
            let deadline = Instant::now() + WAIT;
            vec![normalize_direct_status(
                recv_before(&rx, deadline, "direct replay AwaitingSigner"),
                "n/a",
            )]
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
    // Shared fixed identity with `run_direct_reattach_live` -- see the note
    // there: the reattach `AwaitingSigner` payload is now the frozen
    // author pubkey, and the direct-vs-FFI proof compares it.
    let keys = fixed_keys();
    let engine = NmpEngine::new(ffi_nip65_config()).expect("FFI engine must construct");
    engine
        .set_active_account(Some(keys.public_key().to_hex()))
        .expect("FFI account must activate");

    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: REATTACH_LIVE_KIND,
                    tags: vec![],
                    content: "reattach-live".to_string(),
                    created_at: Some(WRITE_CREATED_AT),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("FFI publish must enqueue");
    let receipt_id = receipt.id();
    let rx = bridge_receipts(&receipt);

    let deadline = Instant::now() + WAIT;
    assert_eq!(
        normalize_ffi_status(
            recv_before(&rx, deadline, "FFI original AwaitingSigner"),
            "n/a"
        ),
        NormStatus::AwaitingSigner(keys.public_key().to_hex())
    );

    let outcome = engine
        .reattach_receipt(receipt_id)
        .expect("FFI reattach call must succeed while the engine is open");
    let norm_outcome = ffi_reattach_outcome(&outcome);
    let replay = match outcome {
        FfiReceiptReattachment::Attached { stream } => {
            let replay_rx = bridge_receipts(&stream);
            let deadline = Instant::now() + WAIT;
            vec![normalize_ffi_status(
                recv_before(&replay_rx, deadline, "FFI replay AwaitingSigner"),
                "n/a",
            )]
        }
        FfiReceiptReattachment::NotFound => {
            panic!("expected Attached for a live retained receipt, got NotFound")
        }
        FfiReceiptReattachment::RetainedButUnreadable => {
            panic!("expected Attached for a live retained receipt, got RetainedButUnreadable")
        }
    };

    let unknown_id_outcome = ffi_reattach_outcome(
        &engine
            .reattach_receipt(u64::MAX)
            .expect("FFI reattach call must succeed while the engine is open"),
    );

    engine.shutdown();
    ReattachProof {
        outcome: norm_outcome,
        replay,
        unknown_id_outcome,
    }
}

// #591: crash-safe correlation parity. Publishing twice with the SAME
// token (a re-composed draft, different body/timestamp the second time)
// must resolve to the SAME receipt id on both surfaces -- never a second
// enqueued write, never a body comparison. `reattach_by_correlation` must
// then behave identically to the existing by-id door for both a known and
// an unknown token.
const CORRELATION_KIND: u16 = 9_994;
const CORRELATION_TOKEN: &str = "parity-crash-safe-correlation-token";

#[derive(Debug, PartialEq, Eq)]
struct CorrelationProof {
    same_receipt_id: bool,
    reattach_outcome: NormReattach,
    unknown_token_outcome: NormReattach,
}

fn run_direct_correlation() -> CorrelationProof {
    let keys = fixed_keys();
    let engine = Engine::new(direct_nip65_config()).expect("direct engine must construct");
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("direct account must activate");

    let token = || {
        Some(
            CorrelationToken::try_from(CORRELATION_TOKEN)
                .expect("token is within the bounded range"),
        )
    };

    let first = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::Custom(CORRELATION_KIND),
                tags: (vec![]).into_iter().collect(),
                content: ("correlation-first").into(),
                created_at: Some(Timestamp::from(WRITE_CREATED_AT)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: token(),
        })
        .expect("direct publish must enqueue");

    // A re-composed draft with a DIFFERENT body and timestamp, same token:
    // must reattach the existing obligation, never enqueue a second write.
    let second = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::Custom(CORRELATION_KIND),
                tags: (vec![]).into_iter().collect(),
                content: ("correlation-second-different-body").into(),
                created_at: Some(Timestamp::from(WRITE_CREATED_AT + 1)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: token(),
        })
        .expect("direct re-publish with the same token must reattach, not fail");
    let same_receipt_id = first.id == second.id;

    let reattach_outcome = direct_reattach_outcome(
        &engine
            .reattach_by_correlation(CORRELATION_TOKEN.to_string())
            .expect("direct reattach-by-correlation must succeed while the engine is open"),
    );
    let unknown_token_outcome = direct_reattach_outcome(
        &engine
            .reattach_by_correlation("never-seen-correlation-token".to_string())
            .expect("direct reattach-by-correlation must succeed while the engine is open"),
    );

    engine.shutdown();
    CorrelationProof {
        same_receipt_id,
        reattach_outcome,
        unknown_token_outcome,
    }
}

fn run_ffi_correlation() -> CorrelationProof {
    let keys = fixed_keys();
    let engine = NmpEngine::new(ffi_nip65_config()).expect("FFI engine must construct");
    engine
        .set_active_account(Some(keys.public_key().to_hex()))
        .expect("FFI account must activate");

    let intent = |content: &str, created_at: u64| FfiWriteIntent {
        payload: FfiWritePayload::Event {
            builder: nmp_ffi::types::FfiEventBuilder {
                kind: CORRELATION_KIND,
                tags: vec![],
                content: content.to_string(),
                created_at: Some(created_at),
            },
        },
        routing: FfiWriteRouting::Auto,
        identity: FfiIdentity::Active,
        correlation: Some(CORRELATION_TOKEN.to_string()),
    };

    let first_id = engine
        .publish(intent("correlation-first", WRITE_CREATED_AT))
        .expect("FFI publish must enqueue")
        .id();

    let second_id = engine
        .publish(intent(
            "correlation-second-different-body",
            WRITE_CREATED_AT + 1,
        ))
        .expect("FFI re-publish with the same token must reattach, not fail")
        .id();
    let same_receipt_id = first_id == second_id;

    let reattach_result = engine
        .reattach_by_correlation(CORRELATION_TOKEN.to_string())
        .expect("FFI reattach-by-correlation must succeed while the engine is open");
    assert_eq!(
        reattach_result.receipt_id,
        Some(first_id),
        "the token must resolve to the SAME receipt id the original publish returned"
    );
    let reattach_outcome = ffi_reattach_outcome(&reattach_result.outcome);
    let unknown_result = engine
        .reattach_by_correlation("never-seen-correlation-token".to_string())
        .expect("FFI reattach-by-correlation must succeed while the engine is open");
    assert_eq!(unknown_result.receipt_id, None);
    let unknown_token_outcome = ffi_reattach_outcome(&unknown_result.outcome);

    engine.shutdown();
    CorrelationProof {
        same_receipt_id,
        reattach_outcome,
        unknown_token_outcome,
    }
}

#[test]
fn direct_and_ffi_correlation_reattach_the_same_obligation_on_token_reuse() {
    let direct = run_direct_correlation();
    let ffi = run_ffi_correlation();
    assert_eq!(
        direct, ffi,
        "direct and FFI correlation reattachment must expose identical outcomes"
    );
    assert!(
        direct.same_receipt_id,
        "a reused token must resolve to the SAME receipt id, never a second enqueued write"
    );
    assert_eq!(direct.reattach_outcome, NormReattach::Attached);
    assert_eq!(direct.unknown_token_outcome, NormReattach::NotFound);
}

#[derive(Debug, PartialEq, Eq)]
struct CancellationProof {
    returned_cancelled: bool,
    observed: Vec<NormStatus>,
}

fn run_direct_cancellation() -> CancellationProof {
    let keys = fixed_keys();
    let engine = Engine::new(direct_nip65_config()).expect("direct engine must construct");
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("direct account must activate");
    let tracked = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::Custom(REATTACH_LIVE_KIND),
                tags: (vec![]).into_iter().collect(),
                content: ("cancel-parity").into(),
                created_at: Some(Timestamp::from(WRITE_CREATED_AT)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("direct publish must enqueue");
    let deadline = Instant::now() + WAIT;
    let mut observed = vec![normalize_direct_status(
        recv_before(
            &tracked.statuses,
            deadline,
            "direct cancellation AwaitingSigner",
        ),
        "n/a",
    )];
    let returned_cancelled = engine
        .cancel(tracked.id)
        .expect("direct cancellation must commit")
        == CancelWriteOutcome::Cancelled;
    observed.push(normalize_direct_status(
        recv_before(
            &tracked.statuses,
            Instant::now() + WAIT,
            "direct cancellation terminal fact",
        ),
        "n/a",
    ));
    assert_eq!(
        tracked.statuses.recv_timeout(WAIT),
        Err(FifoRecvTimeoutError::Closed),
        "direct receipt stream must close after cancellation"
    );
    engine.shutdown();
    CancellationProof {
        returned_cancelled,
        observed,
    }
}

async fn run_ffi_cancellation() -> CancellationProof {
    let keys = fixed_keys();
    let engine = NmpEngine::new(ffi_nip65_config()).expect("FFI engine must construct");
    engine
        .set_active_account(Some(keys.public_key().to_hex()))
        .expect("FFI account must activate");
    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: REATTACH_LIVE_KIND,
                    tags: vec![],
                    content: "cancel-parity".to_string(),
                    created_at: Some(WRITE_CREATED_AT),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("FFI publish must enqueue");
    let receipt_id = receipt.id();
    let rx = bridge_receipts(&receipt);
    let deadline = Instant::now() + WAIT;
    let mut observed = vec![normalize_ffi_status(
        recv_before(&rx, deadline, "FFI cancellation AwaitingSigner"),
        "n/a",
    )];
    let returned_cancelled = engine
        .cancel(receipt_id)
        .expect("FFI cancellation must commit")
        == FfiCancelWriteOutcome::Cancelled;
    observed.push(normalize_ffi_status(
        recv_before(&rx, Instant::now() + WAIT, "FFI cancellation terminal fact"),
        "n/a",
    ));
    assert_eq!(
        rx.recv_timeout(WAIT),
        Err(RecvTimeoutError::Disconnected),
        "FFI receipt stream must close after cancellation"
    );
    engine.shutdown();
    CancellationProof {
        returned_cancelled,
        observed,
    }
}

/// Drain a reattached replay through the write's whole-write terminal.
fn drain_direct_replay(rx: &FifoReceiver<WriteFact>, label: &str) -> Vec<NormStatus> {
    let deadline = Instant::now() + WAIT;
    let mut replay = Vec::new();
    loop {
        let normalized = normalize_direct_status(recv_before(rx, deadline, label), "n/a");
        let done = is_whole_write_terminal(&normalized);
        replay.push(normalized);
        if done {
            return replay;
        }
    }
}

fn drain_ffi_replay(rx: &mpsc::Receiver<FfiWriteFact>, label: &str) -> Vec<NormStatus> {
    let deadline = Instant::now() + WAIT;
    let mut replay = Vec::new();
    loop {
        let normalized = normalize_ffi_status(recv_before(rx, deadline, label), "n/a");
        let done = is_whole_write_terminal(&normalized);
        replay.push(normalized);
        if done {
            return replay;
        }
    }
}

/// TERMINAL half: reach a genuinely terminal retained receipt without the
/// explicit cancellation path exercised above, and prove it survives a
/// restart. Safely-unsent superseded receipts are deliberately destroyed;
/// this fixture first proves a local handoff against an AUTH-gated relay, so
/// the older write owns the narrow, temporary safety receipt that remains.
async fn run_direct_reattach_terminal(path: &std::path::Path) -> ReattachProof {
    let keys = Keys::generate();
    let relay = ScriptedRelay::start(&RelayConfig {
        auth_required_writes: true,
        ..RelayConfig::default()
    })
    .await;
    let relay_url = relay.url.to_string();
    let superseded_id = {
        let engine = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..EngineConfig::default()
        })
        .expect("direct engine must construct");
        engine
            .add_account(&keys.secret_key().to_secret_hex())
            .expect("direct account must register");
        engine
            .set_active_account(Some(keys.public_key()))
            .expect("direct account must activate");
        let write = |content: &str, created_at: u64| WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::Custom(REATTACH_TERMINAL_KIND),
                tags: (vec![]).into_iter().collect(),
                content: content.into(),
                created_at: Some(Timestamp::from(created_at)),
            }),
            routing: WriteRouting::Explicit(vec![relay.url.clone()]),
            identity: Identity::Active,
            correlation: None,
        };
        let first = engine
            .publish(write("reattach-terminal-first", WRITE_CREATED_AT))
            .expect("direct publish must enqueue");
        let parked = collect_direct_receipts_until_awaiting_auth(&first.statuses, &relay_url);
        assert!(
            parked
                .iter()
                .any(|status| matches!(status, NormStatus::Sent(_))),
            "the retained superseded receipt requires an actual local handoff"
        );
        engine
            .publish(write("reattach-terminal-second", WRITE_CREATED_AT + 1))
            .expect("the newer write at the same replaceable coordinate must enqueue");
        assert_eq!(
            drain_direct_replay(&first.statuses, "direct terminal-setup supersession"),
            vec![NormStatus::Superseded],
            "the older attempted write must retire when the newer value wins"
        );
        engine.shutdown();
        first.id
    };
    relay.shutdown();

    let engine = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("direct engine must reopen over the same store");
    let outcome = engine
        .reattach_receipt(superseded_id)
        .expect("direct reattach call must succeed while the engine is open");
    let norm_outcome = direct_reattach_outcome(&outcome);
    let replay = match outcome {
        ReceiptReattachment::Attached { statuses: rx, .. } => {
            drain_direct_replay(&rx, "direct terminal replay")
        }
        _ => panic!("expected Attached for a superseded terminal receipt, got {norm_outcome:?}"),
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
    let relay = ScriptedRelay::start(&RelayConfig {
        auth_required_writes: true,
        ..RelayConfig::default()
    })
    .await;
    let relay_url = relay.url.to_string();
    let superseded_id = {
        let engine = NmpEngine::new(NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..NmpEngineConfig::default()
        })
        .expect("FFI engine must construct");
        engine
            .add_account(keys.secret_key().to_secret_hex())
            .expect("FFI account must register");
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .expect("FFI account must activate");
        let write = |content: &str, created_at: u64| FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: REATTACH_TERMINAL_KIND,
                    tags: vec![],
                    content: content.to_string(),
                    created_at: Some(created_at),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec![relay_url.clone()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        };
        let first = engine
            .publish(write("reattach-terminal-first", WRITE_CREATED_AT))
            .expect("FFI publish must enqueue");
        let receipt_id = first.id();
        let rx = bridge_receipts(&first);
        let parked = collect_ffi_receipts_until_awaiting_auth(&rx, &relay_url);
        assert!(
            parked
                .iter()
                .any(|status| matches!(status, NormStatus::Sent(_))),
            "the retained superseded receipt requires an actual local handoff"
        );
        engine
            .publish(write("reattach-terminal-second", WRITE_CREATED_AT + 1))
            .expect("the newer write at the same replaceable coordinate must enqueue");
        assert_eq!(
            drain_ffi_replay(&rx, "FFI terminal-setup supersession"),
            vec![NormStatus::Superseded],
            "the older attempted write must retire when the newer value wins"
        );
        engine.shutdown();
        receipt_id
    };
    relay.shutdown();

    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..NmpEngineConfig::default()
    })
    .expect("FFI engine must reopen over the same store");
    let outcome = engine
        .reattach_receipt(superseded_id)
        .expect("FFI reattach call must succeed while the engine is open");
    let norm_outcome = ffi_reattach_outcome(&outcome);
    let replay = match outcome {
        FfiReceiptReattachment::Attached { stream } => {
            drain_ffi_replay(&bridge_receipts(&stream), "FFI terminal replay")
        }
        FfiReceiptReattachment::NotFound => {
            panic!("expected Attached for a superseded terminal receipt, got NotFound")
        }
        FfiReceiptReattachment::RetainedButUnreadable => {
            panic!("expected Attached for a superseded terminal receipt, got RetainedButUnreadable")
        }
    };
    let unknown_id_outcome = ffi_reattach_outcome(
        &engine
            .reattach_receipt(u64::MAX)
            .expect("FFI reattach call must succeed while the engine is open"),
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
async fn direct_and_ffi_cancellation_return_and_observe_the_same_terminal_fact() {
    let direct = run_direct_cancellation();
    let ffi = run_ffi_cancellation().await;
    assert_eq!(
        direct, ffi,
        "direct and FFI cancellation must return and stream identical typed facts"
    );
    assert!(direct.returned_cancelled);
    assert_eq!(
        direct.observed,
        vec![
            NormStatus::AwaitingSigner(fixed_keys().public_key().to_hex()),
            NormStatus::NotSent("cancelled"),
        ]
    );
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
         facts for a superseded receipt read back after restart"
    );
    assert_eq!(direct.outcome, NormReattach::Attached);
    assert_eq!(
        direct.replay,
        vec![NormStatus::Superseded],
        "a terminal retained receipt replays its whole-write terminal from disk"
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
    assert_eq!(
        direct_tampered.queue_len, 0,
        "a refused call takes no custody, so no queue entry may exist to inspect"
    );
    assert!(
        direct_tampered.refusal.contains("signature"),
        "the refusal must name the unverifiable signature: {:?}",
        direct_tampered.refusal
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

/// #47: a per-write `Identity::Explicit` naming a registered
/// SECONDARY account (not the active one) must observe the same semantics
/// through the direct Rust facade and the FFI facade: accepted, signed BY
/// THAT KEY, routed via its own outbox, and acked. The
/// `Signed` receipt's event id is the author proof -- an id hashes the
/// author pubkey, so `expected_success_receipts(&override_keys)` can only
/// match if `event.pubkey` IS that key; a silent fallback to the active
/// account on either surface would mint a different id and fail both
/// comparisons.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_identity_publish_signs_as_that_key_identically_direct_and_ffi() {
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
        expected_success_receipts(&override_keys, false),
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
    // `WriteFact` fact verbatim, so both durable kind:3 writes carry the
    // deterministic cold-Nip42-session `awaiting_relay` beat between
    // `routed` and `sent` (see `expected_send_preamble`) — the unfollow too,
    // because worker reconciliation closed the write session when the
    // follow write acked.
    //
    // Asserted per AXIS rather than as one total order. Routing and delivery
    // advance independently. The app relay makes delivery executable, while
    // the author fact remains Unknown because the identically assembled
    // provider's indexer deliberately never answers.
    for (label, seen) in [("follow", &direct.follow), ("unfollow", &direct.unfollow)] {
        assert_eq!(
            delivery_axis(seen),
            vec![
                // #1237: acceptance is `set_following` having enqueued the
                // write, not a fact on its stream.
                "signed",
                "awaiting_relay",
                "awaiting_auth",
                "sent",
                "published"
            ],
            "{label}: the delivery axis is fully ordered on its own: {seen:?}"
        );
        assert_eq!(
            seen.first(),
            Some(&NormFollowActionStatus::Acquiring),
            "{label}: a follow action always opens by acquiring the base list: {seen:?}"
        );
        assert_eq!(
            routing_axis(seen).last(),
            Some(&"routed"),
            "{label}: delivery must not fabricate route settlement: {seen:?}"
        );
    }
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

// ---- #972: explicit routing, direct Rust and FFI ------------------------

/// Publish one write to one relay the caller named, with NO author route
/// seeded anywhere and NO provider assembled. `Auto` would park here, so every relay
/// this write reaches is one the caller chose.
async fn run_direct_explicit_route(keys: &Keys, relay: &ScriptedRelay) -> Vec<NormStatus> {
    let relay_url = relay.url.to_string();
    let engine = Engine::new(EngineConfig {
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

    let unsigned = UnsignedEvent::new(
        pubkey,
        Timestamp::from(WRITE_CREATED_AT),
        Kind::Custom(WRITE_KIND),
        vec![],
        "parity-write",
    );
    let receipt_rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            routing: WriteRouting::Explicit(vec![relay.url.clone()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("direct explicit publish must enqueue")
        .statuses;
    let receipts = collect_direct_receipts(receipt_rx, &relay_url, ReadUntil::WholeWriteTerminal);
    engine.shutdown();
    receipts
}

async fn run_ffi_explicit_route(keys: &Keys, relay: &ScriptedRelay) -> Vec<NormStatus> {
    let relay_url = relay.url.to_string();
    let engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        app_relays: vec![],
        fallback_relays: vec![],
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

    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: WRITE_KIND,
                    tags: vec![],
                    content: "parity-write".to_string(),
                    created_at: Some(WRITE_CREATED_AT),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec![relay_url.clone()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("FFI explicit publish must enqueue");
    let receipt_rx = bridge_receipts(&receipt);
    let receipts = collect_ffi_receipts(&receipt_rx, &relay_url, ReadUntil::WholeWriteTerminal);
    engine.shutdown();
    receipts
}

/// #972 falsifier: an app naming one exact relay gets exactly that relay,
/// identically from direct Rust and across the FFI boundary. Nothing is
/// seeded and no provider runs, so neutral facts have nothing to contribute
/// and cannot be the reason the write lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_publish_to_one_explicitly_named_relay_identically() {
    let keys = fixed_keys();

    let direct_relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let direct = run_direct_explicit_route(&keys, &direct_relay).await;
    direct_relay.shutdown();

    let ffi_relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let ffi = run_ffi_explicit_route(&keys, &ffi_relay).await;
    ffi_relay.shutdown();

    assert_eq!(
        direct, ffi,
        "an explicit route must expose identical ordered receipt facts on both surfaces"
    );
    assert_eq!(
        direct,
        expected_success_receipts(&keys, true),
        "the write routes to exactly the one relay the caller named and is acked there"
    );
}

/// #972 falsifier: `Explicit` with no relays is refused before anything is
/// accepted, on BOTH surfaces, and never degrades into `Auto`. #1237 makes
/// that refusal the CALL's own answer: an instruction that cannot resolve is
/// a refusal, not a parked hope, so no receipt and no queue entry exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_and_ffi_refuse_an_empty_explicit_route_at_the_door() {
    let keys = fixed_keys();

    let engine = Engine::new(EngineConfig::default()).expect("direct engine must construct");
    let pubkey = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("direct account must register")
        .public_key();
    engine
        .set_active_account(Some(pubkey))
        .expect("direct account must activate");
    let direct = match engine.publish(WriteIntent {
        payload: WritePayload::Event(nmp_grammar::EventBuilder {
            kind: Kind::Custom(WRITE_KIND),
            tags: (vec![]).into_iter().collect(),
            content: ("nowhere").into(),
            created_at: Some(Timestamp::from(WRITE_CREATED_AT)),
        }),
        routing: WriteRouting::Explicit(vec![]),
        identity: Identity::Active,
        correlation: None,
    }) {
        Ok(_) => panic!("an empty explicit route must refuse the direct call itself"),
        Err(EngineError::PublishRefused { reason }) => reason,
        Err(other) => panic!("expected EngineError::PublishRefused, got {other:?}"),
    };
    let direct_queue_len = engine
        .publish_queue()
        .expect("the direct engine is open")
        .len();
    engine.shutdown();

    let ffi_engine = NmpEngine::new(NmpEngineConfig {
        store_path: None,
        app_relays: vec![],
        fallback_relays: vec![],
        ..NmpEngineConfig::default()
    })
    .expect("FFI engine must construct");
    let ffi_registration = ffi_engine
        .add_account(keys.secret_key().to_secret_hex())
        .expect("FFI account must register");
    let ffi_pubkey = ffi_registration.public_key();
    ffi_engine
        .set_active_account(Some(ffi_pubkey.clone()))
        .expect("FFI account must activate");
    let ffi = match ffi_engine.publish(FfiWriteIntent {
        payload: FfiWritePayload::Event {
            builder: nmp_ffi::types::FfiEventBuilder {
                kind: WRITE_KIND,
                tags: vec![],
                content: "nowhere".to_string(),
                created_at: Some(WRITE_CREATED_AT),
            },
        },
        routing: FfiWriteRouting::Explicit { relays: vec![] },
        identity: FfiIdentity::Active,
        correlation: None,
    }) {
        Ok(_) => panic!("an empty explicit route must refuse the FFI call itself"),
        Err(FfiError::PublishRefused { reason }) => reason,
        Err(other) => panic!("expected FfiError::PublishRefused, got {other:?}"),
    };
    let ffi_queue_len = ffi_engine
        .publish_queue()
        .expect("the FFI engine is open")
        .len();
    ffi_engine.shutdown();

    assert_eq!(
        direct, ffi,
        "both surfaces must refuse an empty explicit route with the identical typed refusal"
    );
    assert_eq!(
        (direct_queue_len, ffi_queue_len),
        (0, 0),
        "the refusal takes NO custody on either surface -- there is nothing to inspect or remove"
    );
}

/// The same body these fixtures already build, said the way an app says it:
/// a builder states the kind, the tags, the content and (here, so the
/// assertions can name exact ids) the timestamp. The author is not part of
/// it -- the write's identity decides that at acceptance.
fn body_of(unsigned: &nostr::UnsignedEvent) -> nmp_grammar::EventBuilder {
    nmp_grammar::EventBuilder {
        kind: unsigned.kind,
        tags: unsigned.tags.iter().cloned().collect(),
        content: unsigned.content.clone(),
        created_at: Some(unsigned.created_at),
    }
}
