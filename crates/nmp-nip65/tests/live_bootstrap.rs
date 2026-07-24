use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Durability, Engine, EngineConfig, FifoReceiver, FifoRecvTimeoutError, Filter, Kind,
    LiveQuery, PublicKey, RelayUrl, RowDelta, SourceStatus, Timestamp, UnsignedEvent, WriteIntent,
    WritePayload, WriteRouting, WriteStatus,
};
use nmp_nip65::{publish_relay_list_bootstrap, BootstrapRelayList, RelayListEntry, RelayUsage};
use nostr::Keys;
use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    BoxedFuture, Event as RelayEvent, WritePolicy, WritePolicyResult,
};
use tokio::sync::Notify;

fn free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

#[derive(Debug, Default)]
struct HoldState {
    first_write_entered: Notify,
    release_first_write: Notify,
    first_write_claimed: AtomicBool,
}

#[derive(Debug, Clone, Default)]
struct HoldFirstWrite(Arc<HoldState>);

impl WritePolicy for HoldFirstWrite {
    fn admit_event<'a>(
        &'a self,
        _event: &'a RelayEvent,
        _addr: &'a SocketAddr,
    ) -> BoxedFuture<'a, WritePolicyResult> {
        Box::pin(async move {
            if !self.0.first_write_claimed.swap(true, Ordering::AcqRel) {
                self.0.first_write_entered.notify_one();
                self.0.release_first_write.notified().await;
            }
            WritePolicyResult::Accept
        })
    }
}

fn wait_for_status(
    statuses: &FifoReceiver<WriteStatus>,
    timeout: Duration,
    predicate: impl Fn(&WriteStatus) -> bool,
) -> WriteStatus {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for receipt status");
        match statuses.recv_timeout(remaining) {
            Ok(status) if predicate(&status) => return status,
            Ok(_) => {}
            Err(FifoRecvTimeoutError::Lagged) => {
                panic!("the bounded integration receipt must not lag")
            }
            Err(FifoRecvTimeoutError::Timeout | FifoRecvTimeoutError::Closed) => {
                panic!("receipt ended before the expected status")
            }
        }
    }
}

fn ordinary_write(author: PublicKey, content: &str) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Unsigned(UnsignedEvent::new(
            author,
            Timestamp::now(),
            Kind::Metadata,
            vec![],
            content,
        )),
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
        identity_override: None,
        correlation: None,
    }
}

/// The public-facade capstone for #719:
///
/// 1. a real local signer publishes kind:10002 through only the selected
///    bootstrap relay and receives an ordinary tracked receipt;
/// 2. while that real relay is deliberately holding the EVENT before
///    acceptance/broadcast, a subsequent AuthorOutbox write fails because the
///    bootstrap operation did not inject a directory fact from its local
///    pending row;
/// 3. after the relay accepts the EVENT, the already-open ordinary observation
///    receives the relay source/provenance;
/// 4. only then does another ordinary AuthorOutbox write route to and ACK on
///    that relay.
///
/// The test never opens a transport from application code and never writes the
/// directory/store directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_echo_is_the_only_transition_from_bootstrap_to_author_outbox() {
    let port = free_port();
    let policy = HoldFirstWrite::default();
    let relay = LocalRelay::builder()
        .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .port(port)
        .write_policy(policy.clone())
        .build();
    relay.run().await.expect("run local relay");
    let relay_url = RelayUrl::parse(&relay.url().await.to_string()).expect("parse local relay URL");

    let keys = Keys::generate();
    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![relay_url.to_string()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..EngineConfig::default()
    })
    .expect("construct public engine");
    let _registration = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("register ordinary local signer");
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("activate generated account");

    let observation = engine
        .observe(
            LiveQuery::from_filter(Filter {
                kinds: Some(BTreeSet::from([10002])),
                authors: Some(Binding::Literal(BTreeSet::from([keys
                    .public_key()
                    .to_hex()]))),
                ..Filter::default()
            }),
            None,
        )
        .expect("open ordinary relay-list observation");

    let observation_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = observation_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "relay-list observation never reached its real relay"
        );
        let frame = observation
            .recv_timeout(remaining)
            .expect("observation stays open");
        if frame.evidence.sources.iter().any(|source| {
            source.relay == relay_url && matches!(source.status, SourceStatus::Requesting)
        }) {
            break;
        }
    }

    let request = BootstrapRelayList::new(
        keys.public_key(),
        vec![relay_url.clone()],
        vec![RelayListEntry::new(
            relay_url.clone(),
            RelayUsage::ReadWrite,
        )],
    )
    .expect("valid bounded bootstrap request");
    let bootstrap =
        publish_relay_list_bootstrap(&engine, request).expect("tracked publish handoff");

    let routed = wait_for_status(&bootstrap.statuses, Duration::from_secs(10), |status| {
        matches!(status, WriteStatus::Routed(_))
    });
    assert_eq!(
        routed,
        WriteStatus::Routed(BTreeSet::from([relay_url.clone()])),
        "the bootstrap route must contact exactly the selected relay"
    );

    tokio::time::timeout(
        Duration::from_secs(10),
        policy.0.first_write_entered.notified(),
    )
    .await
    .expect("bootstrap EVENT reached the controlled relay");

    let before_echo = engine
        .publish_tracked(ordinary_write(
            keys.public_key(),
            r#"{"name":"before echo"}"#,
        ))
        .expect("ordinary tracked write handoff");
    let failed = wait_for_status(&before_echo.statuses, Duration::from_secs(5), |status| {
        matches!(status, WriteStatus::Failed(_))
    });
    assert!(
        matches!(failed, WriteStatus::Failed(ref reason) if reason.contains("no write relays known for author")),
        "the locally accepted bootstrap row must not become a synthetic routing fact: {failed:?}"
    );

    policy.0.release_first_write.notify_one();
    wait_for_status(
        &bootstrap.statuses,
        Duration::from_secs(10),
        |status| matches!(status, WriteStatus::Acked(relay) if relay == &relay_url),
    );

    let echo_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = echo_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the accepted kind:10002 never returned through the ordinary observation"
        );
        let frame = observation
            .recv_timeout(remaining)
            .expect("observation stays open through relay echo");
        let observed_source = frame.deltas.iter().any(|delta| match delta {
            RowDelta::Added(row) => row.sources.contains(&relay_url),
            RowDelta::SourcesGrew { sources, .. } => sources.contains(&relay_url),
            RowDelta::Removed(_) => false,
        });
        if observed_source {
            break;
        }
    }

    let after_echo = engine
        .publish_tracked(ordinary_write(
            keys.public_key(),
            r#"{"name":"after echo"}"#,
        ))
        .expect("ordinary tracked write after ingest");
    let routed = wait_for_status(&after_echo.statuses, Duration::from_secs(10), |status| {
        matches!(status, WriteStatus::Routed(_))
    });
    assert_eq!(
        routed,
        WriteStatus::Routed(BTreeSet::from([relay_url.clone()])),
        "ordinary AuthorOutbox must now consume the network-ingested NIP-65 fact"
    );
    wait_for_status(
        &after_echo.statuses,
        Duration::from_secs(10),
        |status| matches!(status, WriteStatus::Acked(relay) if relay == &relay_url),
    );

    drop(observation);
    engine.shutdown();
    relay.shutdown();
}
