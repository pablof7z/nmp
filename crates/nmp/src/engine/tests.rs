use std::collections::BTreeSet;
use std::future::Future;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use super::*;
use crate::{Row, RowSignature};
use nmp_engine::core::{Effect, EngineCore, EngineMsg};
use nmp_engine::publish_queue::{NotSentReason, SigningState, WriteFact, WriteOutcome};
use nmp_grammar::Demand;
use nmp_store::RelayObserved;
use nostr::{Keys, Tag};
use std::sync::atomic::{AtomicUsize, Ordering};

fn private_key_bytes(keys: &Keys) -> [u8; 32] {
    keys.secret_key().to_secret_bytes()
}

fn receive_added_row(subscription: &Subscription, event_id: EventId) -> crate::Row {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "canonical row {event_id} did not arrive before the deadline"
        );
        let frame = subscription
            .recv_timeout(remaining)
            .expect("the canonical-row observation stays open");
        if let Some(row) = frame.deltas.into_iter().find_map(|delta| match delta {
            crate::RowDelta::Added(row) if row.id() == event_id => Some(row),
            _ => None,
        }) {
            return row;
        }
    }
}

struct AddPeopleMaterializer;

impl crate::ReplaceableMaterializer for AddPeopleMaterializer {
    fn materialize(
        &self,
        _source: &UnsignedEvent,
        current: &UnsignedEvent,
        operations: &[crate::ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp_grammar::EventBuilder, crate::ReplaceableMaterializerRefusal> {
        let mut tags = current.tags.clone().to_vec();
        for operation in operations {
            let person = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                crate::ReplaceableMaterializerRefusal {
                    reason: error.to_string(),
                }
            })?;
            if !tags
                .iter()
                .any(|tag| tag.as_slice() == ["p", person.to_hex().as_str()])
            {
                tags.push(Tag::public_key(person));
            }
        }
        Ok(nmp_grammar::EventBuilder {
            kind: current.kind,
            tags,
            content: current.content.clone(),
            created_at: None,
        })
    }

    fn materialize_default(
        &self,
        coordinate: &nostr::nips::nip01::Coordinate,
        operations: &[crate::ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp_grammar::EventBuilder, crate::ReplaceableMaterializerRefusal> {
        let mut tags = if coordinate.kind.is_addressable() {
            vec![Tag::identifier(coordinate.identifier.clone())]
        } else {
            Vec::new()
        };
        for operation in operations {
            let person = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                crate::ReplaceableMaterializerRefusal {
                    reason: error.to_string(),
                }
            })?;
            if !tags
                .iter()
                .any(|tag| tag.as_slice() == ["p", person.to_hex().as_str()])
            {
                tags.push(Tag::public_key(person));
            }
        }
        Ok(nmp_grammar::EventBuilder {
            kind: coordinate.kind,
            tags,
            content: String::new(),
            created_at: None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum InitialMaterializerFailure {
    Refusal,
    InvalidCoordinate,
}

impl crate::ReplaceableMaterializer for InitialMaterializerFailure {
    fn materialize(
        &self,
        _source: &UnsignedEvent,
        current: &UnsignedEvent,
        _operations: &[crate::ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp_grammar::EventBuilder, crate::ReplaceableMaterializerRefusal> {
        match self {
            Self::Refusal => Err(crate::ReplaceableMaterializerRefusal {
                reason: "fixture refusal".to_string(),
            }),
            Self::InvalidCoordinate => Ok(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: current.tags.clone().to_vec(),
                content: current.content.clone(),
                created_at: None,
            }),
        }
    }

    fn materialize_default(
        &self,
        coordinate: &nostr::nips::nip01::Coordinate,
        _operations: &[crate::ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp_grammar::EventBuilder, crate::ReplaceableMaterializerRefusal> {
        match self {
            Self::Refusal => Err(crate::ReplaceableMaterializerRefusal {
                reason: "fixture refusal".to_string(),
            }),
            Self::InvalidCoordinate => Ok(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: if coordinate.kind.is_addressable() {
                    vec![Tag::identifier(coordinate.identifier.clone())]
                } else {
                    Vec::new()
                },
                content: String::new(),
                created_at: None,
            }),
        }
    }
}

fn replaceable_contact_intent(
    registration: &crate::RegisteredReplaceableMaterializer,
    base: &Row,
    person: PublicKey,
    destination: RelayUrl,
) -> WriteIntent {
    WriteIntent {
        payload: registration
            .operation(base, person.to_bytes().to_vec())
            .expect("the signed base mints one operation"),
        routing: WriteRouting::Explicit(vec![destination]),
        identity: Identity::Explicit(base.pubkey()),
    }
}

#[test]
fn whole_session_round_trip_is_canonical_and_restores_public_only_accounts() {
    let engine = Engine::new(EngineConfig::default()).expect("engine builds");
    let local = Keys::generate();
    let public_only = Keys::generate().public_key();
    engine
        .add_private_key_account(&private_key_bytes(&local), false)
        .expect("local account");
    engine
        .add_public_key_account(public_only, true)
        .expect("public-only account");
    let first = engine.export_session().expect("export");
    let first_bytes = first.as_bytes().to_vec();
    engine.shutdown();

    let restored =
        Engine::new_with_session(EngineConfig::default(), first).expect("whole session restores");
    let snapshot = restored.session().expect("snapshot");
    assert_eq!(snapshot.current_pubkey, Some(public_only));
    assert_eq!(snapshot.accounts.len(), 2);
    assert!(snapshot.accounts.iter().any(|account| {
        account.public_key == public_only
            && account.provider.is_none()
            && account.signing == crate::SigningAvailability::Unsupported
    }));
    assert!(snapshot.accounts.iter().any(|account| {
        account.public_key == local.public_key()
            && account.provider == Some(crate::SessionProvider::LocalKey)
            && account.signing == crate::SigningAvailability::Available
    }));
    assert_eq!(
        restored.export_session().unwrap().as_bytes(),
        first_bytes.as_slice(),
        "canonical export is deterministic across restart"
    );
    restored.shutdown();
}

#[test]
fn malformed_restore_creates_no_partially_visible_engine() {
    let malformed = crate::SessionPayload::from_bytes(b"not-a-session".to_vec());
    assert!(matches!(
        Engine::new_with_session(EngineConfig::default(), malformed),
        Err(crate::SessionRestoreError::MalformedPayload)
    ));
}

#[test]
fn restored_session_is_installed_before_parked_write_recovery() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("session-before-recovery.redb");
    let keys = Keys::generate();
    let public_key = keys.public_key();
    let config = || EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };

    let receipt_id = {
        let engine = Engine::new(config()).expect("persistent engine");
        engine
            .add_public_key_account(public_key, true)
            .expect("public-only current account");
        let receipt = engine
            .publish(WriteIntent {
                payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: Vec::new().into_iter().collect(),
                    content: "parked before restart".to_string(),
                    created_at: Some(Timestamp::from(55)),
                }),
                routing: nmp_grammar::WriteRouting::Explicit(vec![RelayUrl::parse(
                    "wss://session-recovery.example",
                )
                .unwrap()]),
                identity: Identity::Active,
            })
            .expect("accepted parked write");
        let parked = engine
            .publish_queue_for_event(receipt.event_id, None, 1)
            .unwrap();
        assert!(
            matches!(
                parked[0].signing,
                SigningState::AwaitingSigner { pubkey } if pubkey == public_key
            ),
            "expected parked obligation, got {:?}",
            parked[0].signing
        );
        engine.shutdown();
        receipt.id
    };

    let payload = {
        let engine = Engine::new(EngineConfig::default()).expect("payload engine");
        engine
            .add_private_key_account(&private_key_bytes(&keys), true)
            .expect("persistable local provider");
        let payload = engine.export_session().expect("session payload");
        engine.shutdown();
        payload
    };

    let restarted = Engine::new_with_session(config(), payload).expect("restored engine");
    let session = restarted.session().expect("restored metadata");
    assert_eq!(session.current_pubkey, Some(public_key));
    assert_eq!(session.accounts.len(), 1);
    assert_eq!(
        session.accounts[0].signing,
        crate::SigningAvailability::Available
    );
    let entry = restarted
        .publish_queue(None, 10)
        .expect("recovered queue")
        .into_iter()
        .find(|entry| entry.receipt_id == receipt_id)
        .expect("same accepted obligation");
    assert!(
        matches!(entry.signing, SigningState::Signed { .. }),
        "boot recovery must see the restored provider on its first turn: {:?}",
        entry.signing
    );
    restarted.shutdown();
}

#[test]
fn remove_current_account_clears_current_in_same_runtime_turn() {
    let engine = Engine::new(EngineConfig::default()).expect("engine builds");
    let key = Keys::generate().public_key();
    let account = engine
        .add_public_key_account(key, true)
        .expect("account added and selected");
    assert!(engine.remove_account(&account).expect("remove"));
    let snapshot = engine.session().expect("snapshot");
    assert!(snapshot.accounts.is_empty());
    assert_eq!(snapshot.current_pubkey, None);
    engine.shutdown();
}

#[test]
fn session_mutations_update_one_account_and_clear_the_whole_value() {
    let engine = Engine::new(EngineConfig::default()).expect("engine builds");
    let keys = Keys::generate();
    let public_key = keys.public_key();
    let public_only = engine
        .add_public_key_account(public_key, false)
        .expect("public-only account");
    assert_eq!(public_only.provider, None);
    assert_eq!(public_only.signing, crate::SigningAvailability::Unsupported);

    let enriched = engine
        .add_private_key_account(&private_key_bytes(&keys), true)
        .expect("same account gains local provider");
    assert_eq!(enriched.public_key, public_key);
    assert_eq!(enriched.provider, Some(crate::SessionProvider::LocalKey));
    let snapshot = engine.session().unwrap();
    assert_eq!(snapshot.accounts, vec![enriched]);
    assert_eq!(snapshot.current_pubkey, Some(public_key));

    engine.clear_session().expect("clear whole session");
    assert_eq!(
        engine.session().unwrap(),
        crate::SessionSnapshot {
            accounts: vec![],
            current_pubkey: None
        }
    );
    assert_eq!(
        engine.make_current_account(public_key),
        Err(crate::SessionMutationError::AccountNotFound { public_key })
    );
    engine.shutdown();
}

#[test]
fn removing_or_clearing_session_never_retargets_or_discards_accepted_writes() {
    for clear in [false, true] {
        let engine = Engine::new(EngineConfig::default()).expect("engine builds");
        let public_key = Keys::generate().public_key();
        let account = engine
            .add_public_key_account(public_key, true)
            .expect("public-only current account");
        let query = || {
            LiveQuery::single(Demand {
                selection: nmp_grammar::Filter {
                    kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
                    authors: Some(nmp_grammar::Binding::Literal(BTreeSet::from([
                        public_key.to_hex()
                    ]))),
                    ..nmp_grammar::Filter::default()
                },
                ..Demand::default()
            })
        };
        let before_observation = engine
            .observe(query(), None)
            .expect("author-and-kind-scoped observation opens");
        let receipt = engine
            .publish(WriteIntent {
                payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: Vec::new().into_iter().collect(),
                    content: "accepted before session mutation".to_string(),
                    created_at: Some(Timestamp::from(44)),
                }),
                routing: nmp_grammar::WriteRouting::Explicit(vec![RelayUrl::parse(
                    "wss://accepted.example",
                )
                .unwrap()]),
                identity: Identity::Active,
            })
            .expect("write accepted while signer is absent");
        let receipt_id = receipt.id;
        let frozen_event_id = receipt.event_id;
        drop(receipt.statuses);
        let row_before = receive_added_row(&before_observation, frozen_event_id);
        assert_eq!(row_before.id(), frozen_event_id);
        assert_eq!(row_before.pubkey(), public_key);
        assert_eq!(row_before.kind(), Kind::TextNote);
        assert_eq!(row_before.content(), "accepted before session mutation");
        assert_eq!(row_before.signature(), RowSignature::Pending);
        assert_eq!(row_before.signed_event(), None);
        drop(before_observation);
        let before = engine
            .publish_queue_for_event(frozen_event_id, None, 1)
            .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].pubkey, public_key);
        assert_eq!(
            before[0].signing,
            SigningState::AwaitingSigner { pubkey: public_key }
        );

        if clear {
            engine.clear_session().expect("clear session");
        } else {
            assert!(engine.remove_account(&account).expect("remove account"));
        }

        assert!(engine.session().unwrap().accounts.is_empty());
        assert_eq!(engine.session().unwrap().current_pubkey, None);
        let after_observation = engine
            .observe(query(), None)
            .expect("fresh author-and-kind-scoped observation opens");
        let row_after = receive_added_row(&after_observation, frozen_event_id);
        assert_eq!(row_after.id(), frozen_event_id);
        assert_eq!(row_after.pubkey(), public_key);
        assert_eq!(row_after.kind(), Kind::TextNote);
        assert_eq!(row_after.content(), "accepted before session mutation");
        assert_eq!(row_after.signature(), RowSignature::Pending);
        assert_eq!(row_after.signed_event(), None);
        assert_eq!(
            row_after, row_before,
            "session mutation must preserve the exact canonical row"
        );
        drop(after_observation);
        let ReceiptReattachment::Attached { id, .. } = engine
            .reattach_receipt(receipt_id)
            .expect("reattach receipt")
        else {
            panic!("accepted receipt must remain reattachable after session mutation")
        };
        assert_eq!(id, receipt_id, "reattachment must retain receipt identity");
        let after = engine
            .publish_queue_for_event(frozen_event_id, None, 1)
            .unwrap();
        assert_eq!(after.len(), 1, "accepted receipt remains retained");
        assert_eq!(after[0].receipt_id, receipt_id);
        assert_eq!(after[0].event_id, frozen_event_id);
        assert_eq!(after[0].pubkey, public_key, "frozen author is unchanged");
        assert_eq!(
            after[0].signing,
            SigningState::AwaitingSigner { pubkey: public_key },
            "accepted write remains parked on its frozen author"
        );
        engine.shutdown();
    }
}

fn engine_with_store(store: RedbStore) -> Engine {
    let (engine_thread, handle) = EngineThread::spawn(store, 4, PoolConfig::default())
        .expect("concrete Redb engine construction");
    Engine {
        inner: Mutex::new(Some(Inner {
            handle,
            engine_thread,
        })),
    }
}

#[test]
fn persistent_engine_keeps_healthy_store_usable_after_invariant_fault() {
    use nmp_grammar::{Identity, WritePayload, WriteRouting};
    use nostr::EventBuilder;

    let fixture = tempfile::tempdir().expect("persistent store fixture");
    let path = fixture.path().join("acceptance-invariant.redb");
    let author = Keys::generate();
    let corrupt = EventBuilder::text_note("corrupt canonical acceptance target")
        .sign_with_keys(&author)
        .unwrap();
    {
        let mut store = RedbStore::open(&path).expect("create corruption fixture");
        store
            .insert(
                corrupt.clone(),
                nmp_store::RelayObserved::new(
                    RelayUrl::parse("wss://corruption-source.example").unwrap(),
                    Timestamp::from(1u64),
                ),
            )
            .expect("seed canonical acceptance target");
    }
    nmp_store::testing::corrupt_canonical_event(&path, corrupt.id)
        .expect("store-owned targeted canonical corruption");
    let engine = engine_with_store(RedbStore::open(&path).expect("open corrupted Redb fixture"));
    engine
        .select_test_account(Some(author.public_key()))
        .expect("set facade-owned identity");
    let relay = RelayUrl::parse("wss://invariant.example").unwrap();
    let intent = |event| WriteIntent {
        payload: WritePayload::Signed(event),
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
    };

    let refused = engine.publish(intent(corrupt));
    assert!(
        matches!(&refused, Err(EngineError::PublishRefused { reason }) if reason.contains("decode canonical event")),
        "targeted durable corruption must surface as the store's real invariant: {}",
        refused
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    );
    let ordinary = EventBuilder::text_note("ordinary next write")
        .sign_with_keys(&author)
        .unwrap();
    engine
        .publish(intent(ordinary))
        .expect("a real invariant leaves the healthy Redb handle usable");
    assert_eq!(engine.publish_queue(None, u8::MAX).unwrap().len(), 1);

    engine.shutdown();
}

fn signer_public_key(public_key: PublicKey) -> nmp_signer::SignerPublicKey {
    nmp_signer::SignerPublicKey::new(public_key.to_bytes())
}

fn signer_unsigned_to_nostr(unsigned: nmp_signer::SignerUnsignedEvent) -> nostr::UnsignedEvent {
    let (public_key, created_at, kind, tags, content) = unsigned.into_parts();
    nostr::UnsignedEvent::new(
        PublicKey::from_slice(public_key.as_bytes()).unwrap(),
        Timestamp::from(created_at),
        Kind::from(kind),
        tags.into_iter()
            .map(nostr::Tag::parse)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        content,
    )
}

fn nostr_signed_to_signer(event: nostr::Event) -> nmp_signer::SignerSignedEvent {
    nmp_signer::SignerSignedEvent::new(
        event.id.to_bytes(),
        signer_public_key(event.pubkey),
        event.created_at.as_secs(),
        event.kind.as_u16(),
        event
            .tags
            .to_vec()
            .into_iter()
            .map(nostr::Tag::to_vec)
            .collect(),
        event.content,
        event.sig.serialize(),
    )
}

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn evidence_attribute<'a>(evidence: &'a crate::ObservationEvidence, key: &str) -> Option<&'a str> {
    evidence
        .attributes
        .iter()
        .find_map(|(candidate, value)| (candidate == key).then_some(value.as_str()))
}

#[test]
fn loopback_relay_reaches_the_facade_transport_pool_without_opt_in() {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use nostr::filter::MatchEventOptions;
    use nostr::{ClientMessage, EventBuilder, JsonUtil, RelayMessage};
    use tungstenite::Message;

    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind the intentional local relay");
    let relay_address = listener.local_addr().expect("read local relay address");
    let relay = RelayUrl::parse(&format!("ws://{relay_address}")).expect("parse local relay URL");
    let author = Keys::generate();
    let event = EventBuilder::text_note("facade local relay proof")
        .sign_with_keys(&author)
        .expect("sign relay fixture");
    let expected_id = event.id;

    let relay_thread = std::thread::spawn({
        let event = event.clone();
        move || {
            let (stream, _) = listener.accept().expect("accept facade connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("bound relay read");
            let mut socket = tungstenite::accept(stream).expect("accept WebSocket");
            while let Ok(message) = socket.read() {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(ClientMessage::Req {
                    subscription_id,
                    filters,
                }) = ClientMessage::from_json(text.as_str())
                else {
                    continue;
                };
                if !filters.into_iter().any(|filter| {
                    filter
                        .into_owned()
                        .match_event(&event, MatchEventOptions::new())
                }) {
                    continue;
                }
                socket
                    .send(Message::text(
                        RelayMessage::event(subscription_id.clone().into_owned(), event).as_json(),
                    ))
                    .expect("send matching event");
                socket
                    .send(Message::text(
                        RelayMessage::eose(subscription_id.into_owned()).as_json(),
                    ))
                    .expect("send EOSE");
                socket.flush().expect("flush relay frames");
                while socket.read().is_ok() {}
                return;
            }
            panic!("facade connection ended before a REQ reached the local relay");
        }
    });

    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay.to_string()],
        ..EngineConfig::default()
    })
    .expect("loopback relay must build without opt-in");
    let query = LiveQuery::single(
        crate::Demand::new(
            crate::Filter {
                kinds: Some(BTreeSet::from([1])),
                authors: Some(crate::Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..crate::Filter::default()
            },
            crate::ReadRouting::Explicit(vec![relay]),
        )
        .expect("build pinned local-relay demand"),
    );
    let subscription = engine
        .observe(query, None)
        .expect("observe through supported facade");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    let mut execution = Vec::new();
    while (!found
        || !execution.iter().any(|fact: &crate::ObservationEvidence| {
            fact.kind == "request_settled" && evidence_attribute(fact, "terminal") == Some("eose")
        }))
        && Instant::now() < deadline
    {
        if let Ok(frame) = subscription.recv_timeout(Duration::from_millis(250)) {
            found = frame
                .deltas
                .iter()
                .filter_map(|delta| delta.row().and_then(|row| row.signed_event()))
                .any(|received| received.id == expected_id)
                || found;
            execution.extend(frame.execution);
        }
    }

    subscription.cancel();
    engine.shutdown();
    if !found {
        // Unblock `accept` when the regression under test prevents the
        // engine from dialing at all, so failure stays bounded.
        let _ = std::net::TcpStream::connect(relay_address);
    }
    let relay_result = relay_thread.join();
    assert!(found, "loopback relay never reached the facade query");
    assert!(execution.iter().any(|fact| {
        fact.kind == "concrete_filter"
            && fact.path.as_deref() == Some("$")
            && fact.revision == Some(1)
    }));
    let requests: BTreeSet<_> = execution
        .iter()
        .filter(|fact| fact.kind == "relay_request" && fact.path.as_deref() == Some("$"))
        .map(|fact| {
            (
                fact.revision.expect("request filter revision"),
                evidence_attribute(fact, "transport_generation")
                    .expect("request transport generation")
                    .parse::<u64>()
                    .expect("numeric transport generation"),
                evidence_attribute(fact, "request_revision")
                    .expect("request revision")
                    .parse::<u64>()
                    .expect("numeric request revision"),
            )
        })
        .collect();
    assert!(
        !requests.is_empty(),
        "facade frame must expose an actual REQ handoff"
    );
    assert!(
        execution.iter().any(|fact| {
            fact.kind == "request_settled"
                && fact.path.as_deref() == Some("$")
                && evidence_attribute(fact, "terminal") == Some("eose")
                && requests.contains(&(
                    fact.revision.expect("EOSE filter revision"),
                    evidence_attribute(fact, "transport_generation")
                        .expect("EOSE transport generation")
                        .parse::<u64>()
                        .expect("numeric transport generation"),
                    evidence_attribute(fact, "request_revision")
                        .expect("EOSE request revision")
                        .parse::<u64>()
                        .expect("numeric request revision"),
                ))
        }),
        "EOSE must identify the exact accepted REQ: {execution:#?}"
    );
    relay_result.expect("join local relay");
}

#[test]
fn persistent_store_reset_is_destructive_and_idempotent() {
    let fixture = tempfile::tempdir().expect("temporary directory");
    let path = fixture.path().join("nmp.redb");
    let config = EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };

    let engine = Engine::new(config.clone()).expect("persistent engine must build");
    assert!(
        path.exists(),
        "opening the persistent engine creates its store"
    );
    let before = std::fs::read(&path).expect("live store bytes must be readable");
    let alias = fixture.path().join(".").join("nmp.redb");
    let refusal = Engine::reset_persistent_store(&alias)
        .expect_err("a canonical alias of a live store must refuse reset");
    assert_eq!(
        refusal,
        EngineError::StoreStillOpen {
            path: path
                .canonicalize()
                .expect("live store path must canonicalize")
                .to_string_lossy()
                .into_owned(),
        }
    );
    assert_eq!(
        std::fs::read(&path).expect("refused reset must leave the store readable"),
        before,
        "refused reset must not touch the live store file"
    );
    let hard_link = fixture.path().join("nmp-hard-link.redb");
    std::fs::hard_link(&path, &hard_link).expect("hard-link alias must be created");
    let hard_link_refusal = Engine::reset_persistent_store(&hard_link)
        .expect_err("a hard-link alias of a live store must refuse reset");
    assert_eq!(
        hard_link_refusal,
        EngineError::StoreStillOpen {
            path: hard_link
                .canonicalize()
                .expect("hard-link path must canonicalize")
                .to_string_lossy()
                .into_owned(),
        }
    );
    assert_eq!(
        std::fs::read(&path).expect("hard-link refusal must preserve the original name"),
        before
    );
    assert_eq!(
        std::fs::read(&hard_link).expect("hard-link refusal must preserve the alias"),
        before
    );
    let second_open = Engine::new(config.clone())
        .err()
        .expect("a second persistent engine owner must be refused");
    assert_eq!(
        second_open,
        EngineError::StoreAlreadyOpen {
            path: path
                .canonicalize()
                .expect("live store path must canonicalize")
                .to_string_lossy()
                .into_owned(),
        }
    );

    engine.shutdown();

    let after_shutdown = std::fs::read(&path).expect("shutdown store bytes must remain readable");
    assert_eq!(
        std::fs::read(&hard_link).expect("hard-link alias must match the store after shutdown"),
        after_shutdown
    );
    assert!(matches!(
        Engine::reset_persistent_store(&hard_link),
        Err(EngineError::StoreResetFailed { reason })
            if reason.contains("2 hard links")
    ));
    assert_eq!(
        std::fs::read(&path).expect("multi-link refusal must preserve the original name"),
        after_shutdown
    );
    assert_eq!(
        std::fs::read(&hard_link).expect("multi-link refusal must preserve the alias"),
        after_shutdown
    );
    std::fs::remove_file(&hard_link).expect("restore the single-link reset precondition");
    Engine::reset_persistent_store(&path).expect("a closed store must reset");
    assert!(
        !path.exists(),
        "reset must remove the complete canonical store"
    );
    Engine::reset_persistent_store(&path).expect("a missing store is already reset");

    let reopened = Engine::new(config).expect("reset path must open as a fresh store");
    drop(reopened);
    Engine::reset_persistent_store(&path)
        .expect("dropping an engine must release its store ownership");
}

#[test]
fn failed_persistent_store_open_releases_reset_guard() {
    let fixture = tempfile::tempdir().expect("temporary directory");
    let path = fixture.path().join("corrupt.redb");
    std::fs::write(&path, b"not a redb database").expect("corrupt fixture must write");
    let error = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .err()
    .expect("corrupt store must fail construction");
    assert!(matches!(error, EngineError::StoreOpenFailed { .. }));

    Engine::reset_persistent_store(&path)
        .expect("failed construction must release its store ownership");
    assert!(!path.exists(), "reset must remove the failed-open store");
}

/// #920: the two open refusals an app must never confuse. A store from a
/// superseded epoch is recoverable and the recovery is to discard the
/// file; damaged current-epoch bytes are not, and discarding them
/// destroys the only copy of accepted-but-unpublished writes.
///
/// The epoch fixture is a nonempty store whose marker this build cannot
/// read, so `found` is `None` — "not this epoch", not "no data". The
/// store-owned fixture retains every physical table detail. Retired table
/// names are not recorded here: that list would be knowledge of layouts this
/// repository does not keep.
#[test]
fn superseded_epoch_and_damaged_bytes_are_different_typed_open_refusals() {
    let fixture = tempfile::tempdir().expect("temporary directory");

    let superseded = fixture.path().join("superseded-epoch.redb");
    nmp_store::testing::create_nonempty_markerless_store(&superseded)
        .expect("epoch fixture must create");
    let error = Engine::new(EngineConfig {
        store_path: Some(superseded.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .err()
    .expect("a superseded-epoch store must refuse construction");
    let expected = match &error {
        EngineError::StoreUnsupportedSchema {
            path,
            expected,
            found,
        } => {
            assert!(
                path.ends_with("superseded-epoch.redb"),
                "the refusal must name the store an app would discard: {path}"
            );
            assert_eq!(
                *found, None,
                "a marker this build cannot read is absent, not a different number"
            );
            *expected
        }
        other => {
            panic!("a superseded epoch must not collapse into a generic open failure: {other:?}")
        }
    };
    assert!(expected > 0, "the build's own epoch must be reported");

    // The operator contract (#1017) has to survive the promotion to this
    // boundary, because this is where an app's operator reads it.
    let rendered = error.to_string();
    for required in [
        "discard and recreate this store to continue",
        "NMP can reacquire the relay-backed read cache",
        "accepted but unpublished writes",
        "permanently lost",
    ] {
        assert!(
            rendered.contains(required),
            "the reachable refusal must state {required:?}: {rendered}"
        );
    }

    // The variant tells an app the discard is correct; it is worth
    // nothing if the refused open still owns the file. This is the exact
    // sequence a consumer runs after branching on it.
    Engine::reset_persistent_store(&superseded)
        .expect("the epoch refusal must release its store ownership");
    assert!(
        !superseded.exists(),
        "the discard an app is told to perform must actually be performable"
    );
    Engine::new(EngineConfig {
        store_path: Some(superseded.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("a recreated store must open as the current epoch")
    .shutdown();

    let damaged = fixture.path().join("damaged.redb");
    std::fs::write(&damaged, b"not a redb database").expect("damaged fixture must write");
    let refusal = Engine::new(EngineConfig {
        store_path: Some(damaged.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .err()
    .expect("damaged bytes must refuse construction");
    assert!(
        matches!(refusal, EngineError::StoreOpenFailed { .. }),
        "damaged bytes must never be reported as a discardable epoch: {refusal:?}"
    );
    assert!(
        !refusal.to_string().contains("discard and recreate"),
        "no open refusal but the epoch one may tell an operator to discard: {refusal}"
    );
}

#[test]
fn facade_cancellation_is_typed_idempotent_and_reattachable() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    engine
        .select_test_account(Some(keys.public_key()))
        .expect("engine open");
    let receipt = engine
        .publish(WriteIntent {
            payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (Vec::new()).into_iter().collect(),
                content: ("cancel through facade").into(),
                created_at: Some(Timestamp::from(10)),
            }),
            routing: nmp_grammar::WriteRouting::Auto,
            identity: Identity::Active,
        })
        .expect("accept write");
    // `publish` returning `Ok` IS acceptance -- there is no
    // acceptance fact to wait for on the stream.

    assert_eq!(engine.cancel(receipt.id), Ok(CancelWriteOutcome::Cancelled));
    let mut saw_cancelled = false;
    while let Ok(status) = receipt
        .statuses
        .recv_timeout(std::time::Duration::from_secs(1))
    {
        if status == WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled)) {
            saw_cancelled = true;
            break;
        }
    }
    assert!(saw_cancelled);
    assert_eq!(engine.cancel(receipt.id), Ok(CancelWriteOutcome::Cancelled));

    let ReceiptReattachment::Attached {
        statuses: replay, ..
    } = engine.reattach_receipt(receipt.id).unwrap()
    else {
        panic!("cancelled receipt must remain reattachable")
    };
    assert_eq!(
        replay.recv().unwrap(),
        WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled))
    );
    assert!(matches!(
        engine.cancel(ReceiptId(u64::MAX)),
        Err(CancelWriteError::UnknownReceipt { .. })
    ));

    engine.shutdown();
    assert_eq!(
        engine.cancel(receipt.id),
        Err(CancelWriteError::EngineClosed)
    );
}

#[test]
fn dropping_a_receipt_observer_does_not_cancel_the_write() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    engine
        .select_test_account(Some(keys.public_key()))
        .expect("engine open");
    let receipt = engine
        .publish(WriteIntent {
            payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (Vec::new()).into_iter().collect(),
                content: ("observer lifetime is not write ownership").into(),
                created_at: Some(Timestamp::from(11)),
            }),
            routing: nmp_grammar::WriteRouting::Auto,
            identity: Identity::Active,
        })
        .expect("accept write");
    let receipt_id = receipt.id;
    drop(receipt.statuses);

    let ReceiptReattachment::Attached { .. } = engine.reattach_receipt(receipt_id).unwrap() else {
        panic!("dropping the observer must not remove the receipt")
    };
    assert_eq!(engine.cancel(receipt_id), Ok(CancelWriteOutcome::Cancelled));
    engine.shutdown();
}

#[cfg(feature = "unstable-mechanism")]
#[test]
fn from_parts_cannot_bypass_guard_and_spawn_failure_releases_store() {
    let fixture = tempfile::tempdir().expect("temporary directory");
    let path = fixture.path().join("from-parts.redb");
    let store = RedbStore::open(&path).expect("store must open");
    let engine =
        Engine::from_parts(store, 10, PoolConfig::default()).expect("from_parts engine must build");
    assert!(matches!(
        Engine::reset_persistent_store(&path),
        Err(EngineError::StoreStillOpen { .. })
    ));
    engine.shutdown();
    Engine::reset_persistent_store(&path)
        .expect("from_parts shutdown must release store ownership");

    let store = RedbStore::open(&path).expect("store must reopen");
    let failure = Engine::from_parts(
        store,
        usize::MAX,
        PoolConfig {
            max_relays: usize::MAX,
            ..PoolConfig::default()
        },
    )
    .err()
    .expect("unrepresentable relay envelope must refuse construction");
    assert!(matches!(failure, EngineError::EngineStartFailed { .. }));
    Engine::reset_persistent_store(&path)
        .expect("post-open spawn failure must release RedbStore ownership");
}

#[test]
fn sign_event_returns_exact_verified_event_without_store_or_publish_queue_residue() {
    let fixture = tempfile::tempdir().expect("temporary directory");
    let path = fixture.path().join("sign-only.redb");
    let engine = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("engine must build");
    let secret = format!("{:064x}", 7u8);
    let author = engine
        .install_test_local_provider(&secret)
        .expect("account must register")
        .public_key();
    engine
        .select_test_account(Some(author))
        .expect("account must activate");
    let request = SignEventRequest {
        created_at: nostr::Timestamp::from(1_723_456_789),
        kind: nostr::Kind::Custom(27_272),
        tags: vec![nostr::Tag::parse(vec!["t".to_string(), "sign-only".to_string()]).unwrap()],
        content: "exact body".to_string(),
    };

    let signed = engine
        .sign_event(request.clone())
        .expect("sign-only operation must start")
        .recv()
        .expect("current account's local signing provider must complete");
    assert_eq!(signed.pubkey, author);
    assert_eq!(signed.created_at, request.created_at);
    assert_eq!(signed.kind, request.kind);
    assert_eq!(
        signed.tags.iter().cloned().collect::<Vec<_>>(),
        request.tags
    );
    assert_eq!(signed.content, request.content);
    signed.verify().expect("returned signature must verify");
    engine.shutdown();

    let store = nmp_store::RedbStore::open(&path).expect("store must reopen");
    assert!(
        store
            .query(&nostr::Filter::new())
            .expect("canonical query must succeed")
            .is_empty(),
        "sign-only must not create a canonical row"
    );
    assert!(
        store
            .recover_publish_queue()
            .expect("recover delivery")
            .is_empty(),
        "sign-only must not create an intent, receipt, or delivery lane"
    );
}

#[test]
fn sign_event_rejects_missing_current_account_or_provider_before_invocation() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let active = nostr::Keys::generate().public_key();
    let request = SignEventRequest {
        created_at: nostr::Timestamp::from(1),
        kind: nostr::Kind::TextNote,
        tags: Vec::new(),
        content: "body".to_string(),
    };
    match engine.sign_event(request.clone()) {
        Err(error) => assert_eq!(error, SignEventError::NoCurrentSigningProvider),
        Ok(_) => panic!("a missing current account must refuse before acceptance"),
    }
    engine.select_test_account(Some(active)).unwrap();
    match engine.sign_event(request) {
        Err(error) => assert_eq!(error, SignEventError::NoCurrentSigningProvider),
        Ok(_) => panic!("an unavailable signing provider must refuse before acceptance"),
    }
    engine.shutdown();
}

/// #1657 falsifier: every reader of the current account sees one value.
///
/// The current account used to be stored twice -- `RuntimeSessionState.
/// current_pubkey` and `EngineCore.active_pubkey` -- written by adjacent
/// statements at six sites with nothing typing the pairing. The three readers
/// below were split across those copies: the session snapshot and the
/// sign-event author admission read the runtime's, while `Identity::Active`
/// resolution read the reducer's. A half-written pair would therefore have
/// signed as one account while the reducer authored as another, with no
/// assertion between them.
///
/// This drives a whole account lifecycle -- select, switch, remove, clear --
/// and asserts all three readers move together at every step. Reintroducing a
/// second copy and updating only some of its writers fails here.
#[test]
fn every_current_account_reader_sees_the_same_selection_through_a_full_lifecycle() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let request = || SignEventRequest {
        created_at: nostr::Timestamp::from(1_723_456_789),
        kind: nostr::Kind::Custom(27_273),
        tags: Vec::new(),
        content: "one selection".to_string(),
    };

    // Two provider-backed accounts, so the sign-event admission path is
    // reachable and names a real author.
    let alice = engine
        .install_test_local_provider(&format!("{:064x}", 11u8))
        .expect("alice registers")
        .public_key();
    let bob = engine
        .install_test_local_provider(&format!("{:064x}", 12u8))
        .expect("bob registers")
        .public_key();

    for expected in [alice, bob] {
        engine
            .make_current_account(expected)
            .expect("account becomes current");
        assert_eq!(
            engine.session().expect("session reads").current_pubkey,
            Some(expected),
            "the session snapshot must name the selected account"
        );
        let signed = engine
            .sign_event(request())
            .expect("the current account's provider signs")
            .recv()
            .expect("the sign-only operation completes");
        assert_eq!(
            signed.pubkey, expected,
            "sign-event admission must use the same selection the snapshot reports"
        );
    }

    // Removing the current account clears the selection for every reader.
    engine
        .remove_account(&crate::SessionAccount {
            public_key: bob,
            provider: Some(crate::SessionProvider::LocalKey),
            signing: crate::SigningAvailability::Available,
        })
        .expect("removal succeeds");
    assert_eq!(
        engine.session().expect("session reads").current_pubkey,
        None,
        "removing the current account must clear the snapshot's selection"
    );
    assert_eq!(
        engine.sign_event(request()).err(),
        Some(SignEventError::NoCurrentSigningProvider),
        "removing the current account must clear the sign-event admission's selection"
    );

    // The reducer's own `Identity::Active` resolution is the third reader.
    // A public-key-only account parks the write against the exact author the
    // reducer chose, so the parked obligation names it without a signer race.
    let carol = Keys::generate().public_key();
    engine
        .add_public_key_account(carol, true)
        .expect("public-only current account");
    assert_eq!(
        engine.session().expect("session reads").current_pubkey,
        Some(carol),
        "the snapshot must follow the newest selection"
    );
    let receipt = engine
        .publish(WriteIntent {
            payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: Vec::new().into_iter().collect(),
                content: "authored by the active identity".to_string(),
                created_at: Some(Timestamp::from(77)),
            }),
            routing: nmp_grammar::WriteRouting::Explicit(vec![RelayUrl::parse(
                "wss://one-selection.example",
            )
            .unwrap()]),
            identity: Identity::Active,
        })
        .expect("the active identity resolves and the write is accepted");
    let parked = engine
        .publish_queue_for_event(receipt.event_id, None, 1)
        .expect("the parked obligation reads");
    assert!(
        matches!(
            parked[0].signing,
            SigningState::AwaitingSigner { pubkey } if pubkey == carol
        ),
        "`Identity::Active` must resolve to the same selection every other \
         reader reports, got {:?}",
        parked[0].signing
    );

    engine.clear_session().expect("session clears");
    assert_eq!(
        engine.session().expect("session reads").current_pubkey,
        None,
        "clearing the session must clear the selection for every reader"
    );
    assert_eq!(
        engine.sign_event(request()).err(),
        Some(SignEventError::NoCurrentSigningProvider),
        "clearing the session must leave no signable current account"
    );
    engine.shutdown();
}

struct MismatchedSigner {
    reported: PublicKey,
    actual: Keys,
    calls: Arc<AtomicUsize>,
}

impl nmp_signer::SigningCapability for MismatchedSigner {
    fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
        Some(signer_public_key(self.reported))
    }

    fn sign(
        &self,
        unsigned: nmp_signer::SignerUnsignedEvent,
    ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let unsigned = signer_unsigned_to_nostr(unsigned);
        let substituted = nostr::UnsignedEvent::new(
            self.actual.public_key(),
            unsigned.created_at,
            unsigned.kind,
            unsigned.tags,
            unsigned.content,
        );
        nmp_signer::SignerOp::ok(nostr_signed_to_signer(
            substituted.sign_with_keys(&self.actual).unwrap(),
        ))
    }
}

#[test]
fn sign_event_rejects_mismatched_signer_output() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let reported = nostr::Keys::generate();
    let calls = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(MismatchedSigner {
            reported: reported.public_key(),
            actual: nostr::Keys::generate(),
            calls: Arc::clone(&calls),
        })
        .expect("signer must register");
    engine
        .select_test_account(Some(reported.public_key()))
        .unwrap();
    let request = SignEventRequest {
        created_at: nostr::Timestamp::from(2),
        kind: nostr::Kind::TextNote,
        tags: Vec::new(),
        content: "frozen".to_string(),
    };
    assert!(matches!(
        engine.sign_event(request).unwrap().recv(),
        Err(SignEventError::InvalidSignerOutput { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    engine.shutdown();
}

struct PendingSigner {
    public_key: PublicKey,
    cancellations: Arc<AtomicUsize>,
}

struct NoHookPendingSigner {
    public_key: PublicKey,
    operation: Mutex<Option<nmp_signer::SignerOp<nmp_signer::SignerSignedEvent>>>,
}

impl nmp_signer::SigningCapability for NoHookPendingSigner {
    fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
        Some(signer_public_key(self.public_key))
    }

    fn sign(
        &self,
        _unsigned: nmp_signer::SignerUnsignedEvent,
    ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
        self.operation
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .expect("fixture signs once")
    }
}

struct HookCompletesSigner {
    keys: Keys,
    cancellations: Arc<AtomicUsize>,
}

impl nmp_signer::SigningCapability for HookCompletesSigner {
    fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
        Some(signer_public_key(self.keys.public_key()))
    }

    fn sign(
        &self,
        unsigned: nmp_signer::SignerUnsignedEvent,
    ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
        let signed = nostr_signed_to_signer(
            signer_unsigned_to_nostr(unsigned)
                .sign_with_keys(&self.keys)
                .unwrap(),
        );
        let completion: Arc<
            Mutex<Option<nmp_signer::PendingSignerSender<nmp_signer::SignerSignedEvent>>>,
        > = Arc::new(Mutex::new(None));
        let completion_for_cancel = Arc::clone(&completion);
        let cancellations = Arc::clone(&self.cancellations);
        let (sender, operation) = nmp_signer::SignerOp::pending_channel_with_cancel(move || {
            cancellations.fetch_add(1, Ordering::SeqCst);
            if let Some(sender) = completion_for_cancel
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
            {
                let _ = sender.resolve(Ok(signed));
            }
        });
        *completion
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(sender);
        operation
    }
}

struct CountingSigner {
    keys: Keys,
    calls: Arc<AtomicUsize>,
}

impl nmp_signer::SigningCapability for CountingSigner {
    fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
        Some(signer_public_key(self.keys.public_key()))
    }

    fn sign(
        &self,
        unsigned: nmp_signer::SignerUnsignedEvent,
    ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        nmp_signer::SignerOp::ok(nostr_signed_to_signer(
            signer_unsigned_to_nostr(unsigned)
                .sign_with_keys(&self.keys)
                .unwrap(),
        ))
    }
}

#[test]
fn sign_event_admits_then_invokes_the_signer_exactly_once() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let calls = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(CountingSigner {
            keys: keys.clone(),
            calls: Arc::clone(&calls),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();

    let signed = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(5),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "one slot".to_string(),
        })
        .expect("cap=1 must admit the operation")
        .recv()
        .expect("local signer must complete");
    assert_eq!(signed.pubkey, keys.public_key());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    engine.shutdown();
}

impl nmp_signer::SigningCapability for PendingSigner {
    fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
        Some(signer_public_key(self.public_key))
    }

    fn sign(
        &self,
        _unsigned: nmp_signer::SignerUnsignedEvent,
    ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
        let producer: Arc<
            Mutex<Option<nmp_signer::PendingSignerSender<nmp_signer::SignerSignedEvent>>>,
        > = Arc::new(Mutex::new(None));
        let producer_for_cancel = Arc::clone(&producer);
        let cancellations = Arc::clone(&self.cancellations);
        let (sender, operation) = nmp_signer::SignerOp::pending_channel_with_cancel(move || {
            cancellations.fetch_add(1, Ordering::SeqCst);
            producer_for_cancel
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take();
        });
        *producer.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(sender);
        operation
    }
}

#[test]
fn cancelling_a_write_cancels_its_pending_signer() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let cancellations = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(PendingSigner {
            public_key: keys.public_key(),
            cancellations: Arc::clone(&cancellations),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();

    let publish = |content: &str| {
        engine
            .publish(WriteIntent {
                payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: (Vec::new()).into_iter().collect(),
                    content: content.to_string(),
                    created_at: Some(Timestamp::from(10)),
                }),
                routing: nmp_grammar::WriteRouting::Auto,
                identity: Identity::Active,
            })
            .expect("write must be accepted")
    };

    // #680 removed the native-task census, so the write's pending signer
    // cancellation is observed directly through the `cancellations` counter
    // (bounded poll) rather than the admitted-slot census. The real semantic
    // preserved: cancelling a write cancels its pending signer, and a second
    // write can be published and cancelled the same way.
    let wait_for_cancellations = |target: usize| {
        for _ in 0..500 {
            if cancellations.load(Ordering::SeqCst) >= target {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "expected {target} signer cancellations, saw {}",
            cancellations.load(Ordering::SeqCst)
        );
    };

    let first = publish("cancel cancels the pending signer");
    assert_eq!(engine.cancel(first.id), Ok(CancelWriteOutcome::Cancelled));
    wait_for_cancellations(1);

    let second = publish("a second write cancels the same way");
    assert_eq!(engine.cancel(second.id), Ok(CancelWriteOutcome::Cancelled));
    wait_for_cancellations(2);
    engine.shutdown();
}

#[test]
fn superseding_a_replaceable_write_cancels_its_pending_signer() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let cancellations = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(PendingSigner {
            public_key: keys.public_key(),
            cancellations: Arc::clone(&cancellations),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();

    let publish = |created_at| {
        engine
            .publish(WriteIntent {
                payload: nmp_grammar::WritePayload::Event(
                    nmp_grammar::EventBuilder::new(Kind::Metadata)
                        .content(format!("metadata at {created_at}"))
                        .created_at(Timestamp::from(created_at)),
                ),
                routing: nmp_grammar::WriteRouting::Auto,
                identity: Identity::Active,
            })
            .expect("write must be accepted")
    };

    let wait_for_cancellations = |target: usize| {
        for _ in 0..500 {
            if cancellations.load(Ordering::SeqCst) >= target {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "expected {target} signer cancellations, saw {}",
            cancellations.load(Ordering::SeqCst)
        );
    };

    let first = publish(1);
    let second = publish(2);
    assert_eq!(
        first.statuses.recv().unwrap(),
        WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Superseded))
    );
    wait_for_cancellations(1);

    assert_eq!(engine.cancel(second.id), Ok(CancelWriteOutcome::Cancelled));
    wait_for_cancellations(2);
    engine.shutdown();
}

#[test]
fn sign_event_cancellation_is_session_scoped() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = nostr::Keys::generate();
    let cancellations = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(PendingSigner {
            public_key: keys.public_key(),
            cancellations: Arc::clone(&cancellations),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();
    let request = SignEventRequest {
        created_at: nostr::Timestamp::from(3),
        kind: nostr::Kind::TextNote,
        tags: Vec::new(),
        content: "pending".to_string(),
    };

    let operation = engine.sign_event(request).expect("sign event is admitted");
    operation.cancel_handle().cancel();
    // The cancel hook runs inside `recv_or_cancel` before the operation
    // resolves, so `cancellations == 1` is deterministic once `recv()`
    // observes `Cancelled` (no removed native-task idle barrier needed).
    assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    engine.shutdown();
}

#[test]
fn shutdown_cancels_and_joins_an_accepted_sign_event() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let cancellations = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(PendingSigner {
            public_key: keys.public_key(),
            cancellations: Arc::clone(&cancellations),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();
    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(6),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "shutdown".to_string(),
        })
        .expect("operation must be accepted");

    engine.shutdown();
    assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
}

#[test]
fn sign_event_cancellation_without_adapter_hook_drops_retained_producer_and_joins() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let (producer, operation) = nmp_signer::SignerOp::pending_channel();
    engine
        .install_test_signing_capability(NoHookPendingSigner {
            public_key: keys.public_key(),
            operation: Mutex::new(Some(operation)),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();
    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(7),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "no cancellation hook".to_string(),
        })
        .expect("operation must be accepted");

    operation.cancel_handle().cancel();
    // `recv_or_cancel` sets `receiver = None` before the completion resolves
    // the operation, so once `recv()` observes `Cancelled` the worker
    // receiver is already dropped — deterministic without the removed
    // native-task idle barrier (#680).
    assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
    assert!(
        matches!(
            producer.resolve(Err(nmp_signer::SignerError::Unavailable)),
            Err(nmp_signer::PendingSignerResolveError::ReceiverDropped(_))
        ),
        "the worker receiver must be dropped even while the producer is retained"
    );
    engine.shutdown();
}

#[test]
fn sign_event_shutdown_without_adapter_hook_drops_retained_producer_and_joins() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let (producer, operation) = nmp_signer::SignerOp::pending_channel();
    engine
        .install_test_signing_capability(NoHookPendingSigner {
            public_key: keys.public_key(),
            operation: Mutex::new(Some(operation)),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();
    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(8),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "shutdown without hook".to_string(),
        })
        .expect("operation must be accepted");

    engine.shutdown();
    assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
    assert!(
        matches!(
            producer.resolve(Err(nmp_signer::SignerError::Unavailable)),
            Err(nmp_signer::PendingSignerResolveError::ReceiverDropped(_))
        ),
        "shutdown must drop the worker receiver while the producer is retained"
    );
}

#[test]
fn sign_event_cancellation_claim_beats_hook_that_simultaneously_completes() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let cancellations = Arc::new(AtomicUsize::new(0));
    engine
        .install_test_signing_capability(HookCompletesSigner {
            keys: keys.clone(),
            cancellations: Arc::clone(&cancellations),
        })
        .unwrap();
    engine.select_test_account(Some(keys.public_key())).unwrap();
    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(9),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "cancel wins".to_string(),
        })
        .expect("operation must be accepted");

    operation.cancel_handle().cancel();
    // `recv_or_cancel` fires the cancel hook before the completion resolves
    // the operation, so once `recv()` observes `Cancelled` the hook has run
    // exactly once — no native-task idle barrier is needed (removed in #680).
    assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    engine.shutdown();
}

// #680 deleted `sign_event_capacity_refusal_happens_before_signer_invocation`:
// it asserted the removed global native-task capacity refusal
// (`SignEventError::ExecutorSaturated` + `max_native_tasks`). Sign-event
// admission no longer surfaces a configurable capacity ceiling.
use nmp_grammar::{Identity, WritePayload, WriteRouting};
use nostr::ToBech32;

/// `EngineConfig::default()` (no `store_path`) must select an isolated
/// engine-owned temporary Redb store and construct cleanly with no
/// network at all -- no operator app/fallback relay configured.
#[test]
fn config_with_no_store_path_selects_temporary_redb_store() {
    let engine = Engine::new(EngineConfig::default()).expect("temporary Redb engine must build");
    engine.shutdown();
}

/// A `store_path` must select the on-disk store, opened at that exact
/// path -- the config -> store-selection branch `nmp-ffi` used to hand-roll.
#[test]
fn config_with_store_path_selects_redb_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("engine.redb");
    let config = EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };
    let engine = Engine::new(config).expect("redb-backed engine must build");
    engine.shutdown();
    assert!(path.exists(), "RedbStore::open must have created the file");
}

/// An invalid relay URL in the config is a typed construction error, not
/// a panic.
#[test]
fn config_with_invalid_relay_url_is_a_typed_error() {
    let config = EngineConfig {
        app_relays: vec!["not a url".to_string()],
        ..EngineConfig::default()
    };
    match Engine::new(config) {
        Err(err) => assert_eq!(
            err,
            EngineError::InvalidRelayUrl {
                url: "not a url".to_string()
            }
        ),
        Ok(_) => panic!("a malformed relay URL must fail closed, not construct"),
    }
}

/// The test provider seam must accept both hex and bech32 `nsec` secret keys and
/// return the same public key either way.
#[test]
fn test_provider_seam_accepts_legacy_fixture_encodings() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();

    let via_hex = engine
        .install_test_local_provider(&keys.secret_key().to_secret_hex())
        .expect("hex secret key must parse");
    assert_eq!(via_hex.public_key(), keys.public_key());

    let via_nsec = engine
        .install_test_local_provider(
            &keys
                .secret_key()
                .to_bech32()
                .expect("secret key must encode as bech32"),
        )
        .expect("bech32 nsec must parse");
    assert_eq!(via_nsec.public_key(), keys.public_key());

    engine.shutdown();
}

/// A malformed secret key is a typed error, not a panic.
#[test]
fn test_provider_seam_rejects_malformed_fixture_key() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    assert_eq!(
        engine.install_test_local_provider("not-a-key"),
        Err(crate::SessionMutationError::InvalidSecretKey)
    );
    engine.shutdown();
}

/// One public key is one stable session account. Reinstalling its provider
/// updates that account rather than minting a second identity category.
#[test]
fn same_key_provider_reinstall_updates_one_session_account() {
    let engine = Engine::new(EngineConfig {
        max_auth_capabilities: 1,
        ..EngineConfig::default()
    })
    .expect("engine must build");
    let keys = Keys::generate();
    let first = engine
        .install_test_local_provider(&keys.secret_key().to_secret_hex())
        .expect("first account must register");
    let replacement = engine
        .install_test_local_provider(&keys.secret_key().to_secret_hex())
        .expect("same-key replacement must not consume another slot");

    assert_eq!(first.public_key(), replacement.public_key());
    assert_eq!(first, replacement, "identity is the decoded public key");
    assert_eq!(engine.session().unwrap().accounts.len(), 1);
    assert!(engine.remove_account(&first).unwrap());
    assert!(
        !engine.remove_account(&replacement).unwrap(),
        "whole-account removal is identity-idempotent"
    );
    engine.shutdown();
}

struct AllowAuthPolicy;

impl crate::AuthPolicy for AllowAuthPolicy {
    fn evaluate(&self, _request: crate::AuthPolicyRequest) -> crate::AuthPolicyOp {
        crate::AuthPolicyOp::allow()
    }
}

/// The same exact-instance discipline for AUTH-policy registrations.
#[test]
fn auth_policy_registration_is_exact_instance_repeatable_and_stale_safe() {
    let engine = Engine::new(EngineConfig {
        max_auth_capabilities: 1,
        ..EngineConfig::default()
    })
    .expect("engine must build");
    let public_key = Keys::generate().public_key();
    let first = engine
        .add_auth_policy(public_key, AllowAuthPolicy)
        .expect("first policy must register");
    let replacement = engine
        .add_auth_policy(public_key, AllowAuthPolicy)
        .expect("same-key replacement must not consume another slot");

    assert_eq!(first.expected_public_key(), public_key);
    assert_ne!(first, replacement);
    assert!(
        !engine.remove_auth_policy(&first).unwrap(),
        "a stale policy registration must no-op instead of detaching its replacement"
    );
    assert!(engine.remove_auth_policy(&replacement).unwrap());
    assert!(!engine.remove_auth_policy(&replacement).unwrap());
    engine.shutdown();
}

/// Zero capabilities intentionally admits none, with the typed error.
#[test]
fn zero_auth_capabilities_admits_none_with_typed_error() {
    let engine = Engine::new(EngineConfig {
        max_auth_capabilities: 0,
        ..EngineConfig::default()
    })
    .expect("zero-capability engine must still build");
    assert_eq!(
        engine
            .install_test_local_provider(&Keys::generate().secret_key().to_secret_hex())
            .err(),
        Some(crate::SessionMutationError::CapabilityRegistryFull { limit: 0 })
    );
    assert_eq!(
        engine
            .add_auth_policy(Keys::generate().public_key(), AllowAuthPolicy)
            .err(),
        Some(EngineError::AuthCapabilityRegistryFull { limit: 0 })
    );
    engine.shutdown();
}

/// Accounts and AUTH policies share ONE finite capability ceiling;
/// removing a registration releases its shared slot.
#[test]
fn signer_and_policy_share_one_finite_capability_ceiling() {
    let engine = Engine::new(EngineConfig {
        max_auth_capabilities: 1,
        ..EngineConfig::default()
    })
    .expect("engine must build");
    let keys = Keys::generate();
    let account = engine
        .install_test_local_provider(&keys.secret_key().to_secret_hex())
        .expect("account consumes the one shared slot");
    assert_eq!(
        engine
            .add_auth_policy(keys.public_key(), AllowAuthPolicy)
            .err(),
        Some(EngineError::AuthCapabilityRegistryFull { limit: 1 })
    );
    assert!(engine.remove_account(&account).unwrap());
    engine
        .add_auth_policy(keys.public_key(), AllowAuthPolicy)
        .expect("removing the account releases the shared slot");
    engine.shutdown();
}

/// The account/policy lifecycle verbs fail closed after shutdown like
/// every other verb.
#[test]
fn account_and_policy_lifecycle_fail_closed_after_shutdown() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let account = engine
        .install_test_local_provider(&keys.secret_key().to_secret_hex())
        .expect("account must register");
    let policy = engine
        .add_auth_policy(keys.public_key(), AllowAuthPolicy)
        .expect("policy must register");
    engine.shutdown();

    assert_eq!(
        engine.remove_account(&account).err(),
        Some(crate::SessionMutationError::EngineClosed)
    );
    assert_eq!(
        engine
            .add_auth_policy(keys.public_key(), AllowAuthPolicy)
            .err(),
        Some(EngineError::EngineClosed)
    );
    assert_eq!(
        engine.remove_auth_policy(&policy).err(),
        Some(EngineError::EngineClosed)
    );
}

#[test]
fn sign_event_uses_the_current_account_without_publishing() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let pubkey = engine
        .install_test_local_provider(&keys.secret_key().to_secret_hex())
        .expect("account must register")
        .public_key();
    engine
        .select_test_account(Some(pubkey))
        .expect("account must activate");

    let signed = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(1_750_000_000),
            kind: Kind::Custom(27_235),
            tags: vec![Tag::parse(["client", "nip07-test"]).expect("valid tag")],
            content: "sign without publish".to_string(),
        })
        .expect("current account's local signing provider must start")
        .recv()
        .expect("current account's local signing provider must sign");

    assert_eq!(signed.pubkey, pubkey);
    assert_eq!(signed.created_at, Timestamp::from(1_750_000_000));
    assert_eq!(signed.kind, Kind::Custom(27_235));
    assert_eq!(signed.content, "sign without publish");
    assert!(signed.verify().is_ok());
    engine.shutdown();
}

#[test]
fn sign_event_without_a_current_account_fails_closed() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let result = engine.sign_event(SignEventRequest {
        created_at: Timestamp::from(1_750_000_000),
        kind: Kind::TextNote,
        tags: Vec::new(),
        content: "unsigned".to_string(),
    });
    match result {
        Err(error) => assert_eq!(error, SignEventError::NoCurrentSigningProvider),
        Ok(_) => panic!("a missing current account must fail closed"),
    }
    engine.shutdown();
}

/// #52's headline falsifier, exercised through the facade: a tampered
/// `WritePayload::Signed` is rejected at `EngineCore::on_publish`'s
/// acceptance boundary (Unit A0) regardless of entry point -- the
/// receipt stream this facade's `publish` returns delivers `Failed` as
/// its FIRST and ONLY status, with no preceding `Accepted` and no
/// relay ever contacted (this test configures zero relays, so any
/// routing attempt would hang/panic rather than silently pass).
#[test]
fn tampered_signed_publish_fails_closed_with_no_accepted() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    // An arbitrary caller-owned kind, not any NIP-01 core schema --
    // docs/known-gaps.md's v2-contract promotion forbids baking a
    // kind:1-first bias into the facade's own acceptance fixtures.
    let mut event = nostr::EventBuilder::new(nostr::Kind::Custom(9999), "original")
        .sign_with_keys(&keys)
        .expect("test fixture must sign cleanly");
    // Tamper the content after signing: id/sig no longer match it, but
    // the event otherwise still looks well-formed.
    event.content = "tampered".to_string();

    let refused = engine.publish(WriteIntent {
        payload: WritePayload::Signed(event),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    });
    assert!(
        matches!(
            refused.as_ref().err(),
            Some(EngineError::PublishRefused { .. })
        ),
        "a forged Signed payload must refuse the call itself, taking nothing \
         into custody -- got {:?}",
        refused.as_ref().err()
    );

    engine.shutdown();
}

/// #47 falsifier (a) through the facade: with account A current and B
/// merely registered, never activated, a
/// builder carrying `Identity::Explicit(B)` reaches
/// `WriteFact::Signed` bearing the exact id of the frozen B-authored
/// body -- which commits cryptographically to author and content --
/// and the session still answers A afterward: naming B
/// consented to ONE write, it never re-rooted the engine.
#[test]
fn an_explicit_identity_publishes_as_a_secondary_without_moving_the_current_account() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let pk_a = engine
        .install_test_local_provider(&keys_a.secret_key().to_secret_hex())
        .expect("account A must register")
        .public_key();
    let pk_b = engine
        .install_test_local_provider(&keys_b.secret_key().to_secret_hex())
        .expect("account B must register")
        .public_key();
    engine
        .select_test_account(Some(pk_a))
        .expect("account A must activate");

    let draft = nostr::UnsignedEvent::new(
        pk_b,
        Timestamp::from(1_750_000_047),
        Kind::Custom(9999),
        Vec::new(),
        "one write as b, engine still rooted on a",
    );
    let expected = draft
        .clone()
        .sign_with_keys(&keys_b)
        .expect("derive the frozen body's id");
    let rx = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: draft.kind,
                tags: draft.tags.iter().cloned().collect(),
                content: draft.content.clone(),
                created_at: Some(draft.created_at),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(pk_b),
        })
        .expect("engine is open")
        .statuses;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut signed_as_b = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(WriteFact::Signing(SigningState::Signed { event_id: id })) => {
                assert_eq!(
                    id, expected.id,
                    "Signed must carry the frozen B-authored body's exact id"
                );
                signed_as_b = true;
                break;
            }
            Ok(WriteFact::Signing(SigningState::Refused { reason })) => {
                panic!("override publish must not be refused by the signer: {reason}")
            }
            Ok(WriteFact::Outcome(outcome)) => {
                panic!("override publish must not terminate pre-routing: {outcome:?}")
            }
            Ok(_) => {}
            Err(nmp_runtime::FifoRecvTimeoutError::Timeout) => {}
            Err(nmp_runtime::FifoRecvTimeoutError::Closed) => break,
            Err(nmp_runtime::FifoRecvTimeoutError::Lagged) => {
                panic!("short identity-override receipt unexpectedly lagged")
            }
        }
    }
    assert!(signed_as_b, "override publish must reach Signed as B");
    assert_eq!(
        engine.test_current_public_key().expect("engine is open"),
        Some(pk_a),
        "the per-write override must never move the current account"
    );

    engine.shutdown();
}

/// `shutdown` must be safe to call more than once -- a second call
/// finds `inner` already taken and no-ops rather than panicking.
#[test]
fn shutdown_is_idempotent() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    engine.shutdown();
    engine.shutdown();
}

/// Every verb must fail closed with `EngineClosed` after `shutdown` --
/// never panic, never silently hand back a dead-on-arrival value. This
/// is the fix for the review finding that `observe`/`observe_diagnostics`
/// used to panic through `Handle`'s internal `.expect(...)` once the
/// engine thread had actually exited, and `publish` used to silently
/// return an already-disconnected receiver with no signal that the
/// engine was closed.
#[test]
fn every_verb_fails_closed_after_shutdown() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    engine.shutdown();

    assert_eq!(
        engine.observe(probe_query(), None).err(),
        Some(EngineError::EngineClosed)
    );
    assert_eq!(
        engine.observe_diagnostics().err(),
        Some(EngineError::EngineClosed)
    );
    assert_eq!(
        engine.observe(probe_query(), Some(window_probe())).err(),
        Some(EngineError::EngineClosed)
    );
    assert_eq!(
        engine.select_test_account(None).err(),
        Some(EngineError::EngineClosed)
    );
    assert_eq!(
        engine.install_test_local_provider(&Keys::generate().secret_key().to_secret_hex()),
        Err(crate::SessionMutationError::EngineClosed)
    );
    let publish_result = engine.publish(WriteIntent {
        payload: WritePayload::Event(nmp_grammar::EventBuilder {
            kind: nostr::Kind::Custom(9999),
            tags: (Vec::new()).into_iter().collect(),
            content: ("unreachable").into(),
            created_at: Some(nostr::Timestamp::now()),
        }),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    });
    assert_eq!(publish_result.err(), Some(EngineError::EngineClosed));
}

#[test]
fn initial_materializer_failures_leave_no_acceptance_residue() {
    for (slot, failure, expected) in [
        (31u8, InitialMaterializerFailure::Refusal, "fixture refusal"),
        (
            51,
            InitialMaterializerFailure::InvalidCoordinate,
            "materializer changed the replaceable coordinate",
        ),
    ] {
        let spec = crate::ReplaceableMaterializerSpec::new([slot; 16], [slot + 1; 16], failure);
        let registration = spec.handle();
        let engine = Engine::new_with_capabilities(EngineConfig::default(), vec![spec])
            .expect("engine must build");
        let author = Keys::generate();
        engine
            .add_private_key_account(&private_key_bytes(&author), true)
            .expect("author is available");
        let base_event = nostr::EventBuilder::new(Kind::ContactList, "base")
            .custom_created_at(Timestamp::from(1))
            .sign_with_keys(&author)
            .expect("base is signed");
        let base = Row::from_relay_event(base_event, BTreeSet::new());
        let intent = replaceable_contact_intent(
            &registration,
            &base,
            Keys::generate().public_key(),
            RelayUrl::parse("wss://initial-refusal.example").unwrap(),
        );
        let error = engine
            .publish(intent)
            .err()
            .expect("failure mode refuses publication");
        assert!(
            matches!(&error, EngineError::PublishRefused { reason } if reason.contains(expected)),
            "{failure:?} must remain a typed pre-custody refusal: {error:?}"
        );
        assert!(
            engine
                .publish_queue(None, u8::MAX)
                .expect("queue remains readable")
                .is_empty(),
            "{failure:?} must create no receipt, signing, routing, or delivery state"
        );
        let observation = engine
            .observe(
                LiveQuery::single(Demand {
                    selection: nmp_grammar::Filter {
                        kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                        ..nmp_grammar::Filter::default()
                    },
                    ..Demand::default()
                }),
                None,
            )
            .expect("post-refusal observation opens");
        let opening = observation
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("post-refusal observation receives its opening frame");
        assert!(
            opening.deltas.iter().all(|delta| delta.row().is_none()),
            "{failure:?} must create no optimistic row"
        );
        engine.shutdown();
    }
}

#[test]
fn missing_compiled_capability_refuses_open_and_leaves_the_store_unchanged() {
    let directory = tempfile::tempdir().expect("persistent test directory");
    let path = directory.path().join("missing-capability.redb");
    let config = EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };
    let spec = crate::ReplaceableMaterializerSpec::new([81; 16], [82; 16], AddPeopleMaterializer);
    let registration = spec.handle();
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    {
        let engine = Engine::new_with_capabilities(config.clone(), vec![spec])
            .expect("engine with the compiled capability opens");
        engine
            .add_private_key_account(&private_key_bytes(&author), true)
            .expect("author is available");
        engine
            .publish(WriteIntent {
                payload: registration
                    .first_value_operation(
                        Kind::ContactList,
                        String::new(),
                        alice.to_bytes().to_vec(),
                    )
                    .expect("the first-value follow is complete"),
                routing: WriteRouting::Explicit(vec![RelayUrl::parse(
                    "wss://missing-capability.example",
                )
                .unwrap()]),
                identity: Identity::Explicit(author.public_key()),
            })
            .expect("the follow enters custody");
        assert_eq!(
            engine
                .publish_queue(None, u8::MAX)
                .expect("queue reads")
                .len(),
            1
        );
        engine.shutdown();
    }
    let error = match Engine::new(config.clone()) {
        Ok(_) => panic!("missing compiled capability must refuse open"),
        Err(error) => error,
    };
    assert!(
        matches!(error, EngineError::MissingReplaceableCapability { program, format } if program == [81; 16] && format == [82; 16]),
        "open must name the missing compiled capability: {error:?}"
    );
    // The refused open started no engine and must not mutate the durable
    // semantic content. Reopen WITH the capability and prove the retained
    // work is intact: the same single queue entry survives the refused open.
    // (A raw file-length check admits a same-size mutation; a raw byte
    // compare conflates redb's own open-time lock/journal churn.)
    let spec = crate::ReplaceableMaterializerSpec::new([81; 16], [82; 16], AddPeopleMaterializer);
    let reopened =
        Engine::new_with_capabilities(config, vec![spec]).expect("reopen with the capability");
    assert_eq!(
        reopened
            .publish_queue(None, u8::MAX)
            .expect("queue reads after the refused open")
            .len(),
        1,
        "the refused open must not drop the retained follow"
    );
    reopened.shutdown();
}

/// #1624 falsifier: trusted capability materialization must not spawn an OS
/// thread. The process-wide thread census is a monotonic counter shared by
/// every parallel test in this binary, so sampling it here is unsafe — other
/// engines' construction would inflate it between samples. The inner test
/// runs in a child process that executes ONLY it (exact filter), isolating
/// the counter. Restoring a per-call materializer spawn makes the inner
/// assertion fail.
#[test]
fn repeated_materializations_do_not_change_the_process_thread_count() {
    let exe = std::env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "engine::tests::repeated_materializations_do_not_change_the_process_thread_count_inner",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .expect("spawn the isolated census test");
    assert!(
        output.status.success(),
        "inner census test failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn duplicate_replaceable_capability_refuses_before_store_open() {
    let fixture = tempfile::tempdir().expect("temporary directory");
    let path = fixture.path().join("duplicate-capability.redb");
    let config = EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };
    let capabilities = vec![
        crate::ReplaceableMaterializerSpec::new([85; 16], [86; 16], AddPeopleMaterializer),
        crate::ReplaceableMaterializerSpec::new([85; 16], [86; 16], AddPeopleMaterializer),
    ];

    let error = Engine::new_with_capabilities(config, capabilities)
        .err()
        .expect("duplicate capability identity must refuse construction");
    let EngineError::DuplicateReplaceableCapability { program, format } = error else {
        panic!("duplicate capability returned the wrong construction error")
    };
    assert_eq!(program, [85; 16]);
    assert_eq!(format, [86; 16]);
    assert!(
        !path.exists(),
        "duplicate capability refusal must happen before store custody"
    );
}

#[test]
#[ignore = "ran only by the outer census test in an isolated child process"]
fn repeated_materializations_do_not_change_the_process_thread_count_inner() {
    let spec = crate::ReplaceableMaterializerSpec::new([83; 16], [84; 16], AddPeopleMaterializer);
    let registration = spec.handle();
    let engine = Engine::new_with_capabilities(EngineConfig::default(), vec![spec])
        .expect("engine must build");
    let author = Keys::generate();
    engine
        .add_public_key_account(author.public_key(), true)
        .expect("author is available without a signer");
    let destination = RelayUrl::parse("wss://thread-census.example").unwrap();
    let before = crate::nmp_threads_spawned();
    let first = engine
        .publish(WriteIntent {
            payload: registration
                .first_value_operation(
                    Kind::ContactList,
                    String::new(),
                    Keys::generate().public_key().to_bytes().to_vec(),
                )
                .expect("the first-value follow is complete"),
            routing: WriteRouting::Explicit(vec![destination.clone()]),
            identity: Identity::Explicit(author.public_key()),
        })
        .expect("first follow enters custody");
    let observation = engine
        .observe(
            LiveQuery::single(Demand {
                selection: nmp_grammar::Filter {
                    kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                    ..nmp_grammar::Filter::default()
                },
                ..Demand::default()
            }),
            None,
        )
        .expect("contact-list observation opens");
    let current = receive_added_row(&observation, first.event_id);
    engine
        .publish(replaceable_contact_intent(
            &registration,
            &current,
            Keys::generate().public_key(),
            destination,
        ))
        .expect("second follow enters custody");

    let relay = RelayUrl::parse("wss://thread-census-source.example").unwrap();
    let source_author = Keys::generate();
    let base = nostr::EventBuilder::new(Kind::ContactList, "base")
        .custom_created_at(Timestamp::from(10))
        .sign_with_keys(&source_author)
        .expect("base source signs");
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            base.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(10)),
        )
        .expect("base source is stored");
    let transaction_probe =
        nmp_store::testing::arm_materializer_entry_transaction_probe(&mut store, 2);
    let mut core = EngineCore::new(store, 4);
    core.install_replaceable_materializers(vec![crate::ReplaceableMaterializerSpec::new(
        [87; 16],
        [88; 16],
        AddPeopleMaterializer,
    )]);
    core.handle(EngineMsg::SetActivePubkey(Some(source_author.public_key())));
    let operation = nmp_grammar::ReplaceableOperation::from_registered_parts(
        [87; 16],
        [88; 16],
        UnsignedEvent::from(base.clone()),
        UnsignedEvent::from(base),
        Keys::generate().public_key().to_bytes().to_vec(),
    )
    .expect("successor fixture operation is valid");
    assert!(core
        .handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(operation),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
        }))
        .iter()
        .any(|effect| matches!(effect, Effect::WriteAccepted(..))));
    let successor = nostr::EventBuilder::new(Kind::ContactList, "newer source")
        .custom_created_at(Timestamp::from(20))
        .sign_with_keys(&source_author)
        .expect("successor source signs");
    let mut successor_effects = Vec::new();
    core.ingest_relay_events(
        vec![(successor, RelayObserved::new(relay, Timestamp::from(20)))],
        &mut successor_effects,
    );
    transaction_probe.assert_exhausted();

    let after = crate::nmp_threads_spawned();
    assert_eq!(
        before, after,
        "trusted materialization must not spawn an OS thread"
    );
    engine.shutdown();
}

/// A second, concurrent `shutdown` racing the first must still only
/// ever see the gate flip exactly once -- both calls are safe, and
/// after both return the engine is closed exactly as if only one had
/// been called.
#[test]
fn concurrent_shutdown_calls_are_race_free() {
    let engine = Arc::new(Engine::new(EngineConfig::default()).expect("engine must build"));
    let other = Arc::clone(&engine);
    let joined = std::thread::spawn(move || other.shutdown());
    engine.shutdown();
    joined.join().expect("concurrent shutdown must not panic");

    assert_eq!(
        engine.select_test_account(None).err(),
        Some(EngineError::EngineClosed)
    );
}

/// Dropping an `Engine` that was never explicitly `shutdown` must not
/// panic and must still run the same teardown path (the review's
/// RAII-shutdown blocker: a bare `Mutex<Option<Inner>>` drop would
/// detach `EngineThread`'s join handles while `engine_loop` kept
/// running with `self_inbox` still open). This variant has no live
/// observer at all; [`drop_with_live_observers_tears_down_within_bound_and_disconnects_cleanly`]
/// below is the same claim with a query AND a diagnostics subscription
/// still open at drop time.
#[test]
fn drop_without_explicit_shutdown_does_not_panic() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    drop(engine);
}

/// The RAII-shutdown claim, proven with LIVE handles rather than an
/// idle engine: drop an `Engine` while a query [`Subscription`] AND a
/// [`DiagnosticsSubscription`] are still open, and prove (a) `Drop`'s
/// `shutdown`+`join` completes within a bounded wait rather than
/// hanging -- the regression this whole fix guards against is
/// detaching `EngineThread`'s join handles while `engine_loop` kept
/// running with live subscribers still registered; (b) both channels
/// observe a clean disconnect afterward, not a hang; (c) dropping the
/// surviving handles once the engine is already gone does not panic --
/// `Handle::unsubscribe`/`DiagnosticsHandle::cancel` are already
/// fire-and-forget (`let _ = self.inbox.send(...)`), so this pins that
/// tolerance holds end-to-end through a real `Drop`, not only in
/// isolation.
///
/// The bound in (a) is enforced by dropping `engine` on a WORKER
/// thread and awaiting its completion signal via
/// `Receiver::recv_timeout` on THIS thread -- not by dropping inline
/// and checking elapsed time afterward. A synchronous inline `drop`
/// that deadlocked inside `shutdown`+`join` would never reach an
/// elapsed-time check at all, so that shape is not a real liveness
/// bound (it only hangs until the outer test-runner's own timeout);
/// `recv_timeout` is what turns a `Drop` deadlock into an ordinary
/// assertion failure here instead.
#[test]
fn drop_with_live_observers_tears_down_within_bound_and_disconnects_cleanly() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");

    let subscription = engine.observe(probe_query(), None).expect("engine is open");
    let diagnostics = engine.observe_diagnostics().expect("engine is open");

    // Drain the one proactive delivery each stream makes on open (a
    // fresh subscribe always gets one -- possibly empty -- batch;
    // `observe_diagnostics` delivers the CURRENT snapshot immediately)
    // so the post-drop assertions below observe a disconnect, not
    // leftover backlog.
    subscription
        .recv()
        .expect("a fresh subscribe delivers one batch before anything else happens");
    diagnostics
        .recv()
        .expect("observe_diagnostics delivers the current snapshot immediately");

    // Drop `engine` on a WORKER thread and signal completion over a
    // channel, rather than dropping it inline on this thread and
    // checking elapsed time afterward -- a synchronous `drop` that
    // deadlocked inside `shutdown`+`join` would never reach an
    // `elapsed` check at all, so that shape isn't a real liveness
    // bound (it just hangs until the outer test-runner's own
    // timeout). `recv_timeout` on THIS thread is what makes a `Drop`
    // deadlock trip the bound as an ordinary assertion failure
    // instead of a hang.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(engine);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("Drop must tear EngineThread down within a bounded wait, not hang");

    match subscription.recv() {
        Err(_) => {}
        Ok(msg) => panic!(
            "query channel must disconnect once the dropped engine's thread has \
             fully exited, got another batch instead: {msg:?}"
        ),
    }
    assert!(
        diagnostics.recv().is_none(),
        "diagnostics channel must disconnect (None) once the engine is dropped"
    );

    // Both surviving handles' own `Drop` (unsubscribe/cancel) must not
    // panic even though the engine that owned them is already gone.
    drop(subscription);
    drop(diagnostics);
}

/// codex-nova's non-negotiable proof #1: `ObservationCancel::cancel()`
/// called from ANOTHER handle must unblock a drain loop genuinely
/// parked inside `Subscription::recv()`, within a bounded wait -- not
/// rely on that loop's own next `recv()` call to eventually notice a
/// disconnect on its own timescale. This is exactly the shape
/// `nmp-ffi`'s drain thread depends on: it owns the `Subscription`
/// (`recv()` blocks, so nothing else can), while a caller-held
/// `cancel_handle()` clone triggers withdrawal from elsewhere.
#[test]
fn cancel_handle_unblocks_a_genuinely_blocked_recv_within_a_bound() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let subscription = engine.observe(probe_query(), None).expect("engine is open");

    // Drain the one proactive delivery a fresh subscribe always makes,
    // so the drain thread's `recv()` below has nothing already queued
    // and must genuinely block.
    subscription
        .recv()
        .expect("a fresh subscribe delivers one batch before anything else happens");

    let cancel = subscription.cancel_handle();

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // No further events are ever published against this probe
        // query (no relays configured, arbitrary caller-owned kind) --
        // absent cancellation, this call blocks forever.
        let terminal = loop {
            match subscription.recv() {
                Ok(frame)
                    if frame
                        .execution
                        .iter()
                        .any(|evidence| evidence.kind == "withdrawn") =>
                {
                    break true;
                }
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        let disconnected = subscription.recv().is_err();
        let _ = result_tx.send((terminal, disconnected));
    });

    cancel.cancel();

    let (terminal, disconnected) = result_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect(
            "cancel() from a separate handle must unblock the drain thread's recv() \
             within a bounded wait, not hang",
        );
    assert!(
        terminal,
        "the unblocked recv() must expose the observation's terminal Withdrawn fact"
    );
    assert!(
        disconnected,
        "the receive after Withdrawn must observe a deterministic disconnect"
    );

    engine.shutdown();
}

#[test]
fn history_cancel_handle_unblocks_idle_recv_within_a_bound() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let subscription = engine
        .observe(probe_query(), Some(window_probe()))
        .expect("engine is open");
    subscription
        .recv()
        .expect("a fresh windowed subscription delivers its current state");
    let cancel = subscription.cancel_handle();

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = result_tx.send(subscription.recv().is_err());
    });
    cancel.cancel();
    assert!(result_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("history cancellation must wake the blocked receiver"));
    engine.shutdown();
}

#[test]
fn shutdown_wakes_a_live_history_receiver_within_a_bound() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let subscription = engine
        .observe(probe_query(), Some(window_probe()))
        .expect("engine is open");
    subscription
        .recv()
        .expect("a fresh windowed subscription delivers its current state");

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = result_tx.send(subscription.recv().is_err());
    });
    engine.shutdown();
    assert!(result_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("shutdown must wake the blocked history receiver"));
}

#[test]
fn history_advance_and_blocking_recv_have_safe_split_ownership() {
    let fixture = tempfile::tempdir().expect("temporary directory");
    let path = fixture.path().join("history-advance.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://history-facade.example").unwrap();
    {
        let mut store = RedbStore::open(&path).expect("history store must open");
        for index in 0..3 {
            let event = UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(100),
                Kind::Custom(7_777),
                Vec::new(),
                format!("history-{index}"),
            )
            .sign_with_keys(&keys)
            .unwrap();
            store
                .insert(
                    event,
                    nmp_store::RelayObserved::new(relay.clone(), Timestamp::from(200)),
                )
                .unwrap();
        }
    }

    let engine = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("engine must build");
    let query = LiveQuery::single(Demand {
        selection: nmp_grammar::Filter {
            kinds: Some(std::collections::BTreeSet::from([7_777])),
            authors: Some(nmp_grammar::Binding::Literal(
                std::collections::BTreeSet::from([keys.public_key().to_hex()]),
            )),
            ..nmp_grammar::Filter::default()
        },
        ..Demand::default()
    });
    let window = Window::Expandable {
        initial: std::num::NonZeroUsize::new(1).unwrap(),
        max: std::num::NonZeroUsize::new(3).unwrap(),
    };
    let subscription = engine
        .observe(query, Some(window))
        .expect("window must open");
    subscription.recv().expect("initial frame must arrive");
    let window_handle = subscription
        .window_handle()
        .expect("a windowed observation exposes a window handle");

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (batch_tx, batch_rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        loop {
            let frame = subscription.recv();
            let returned = matches!(
                frame
                    .as_ref()
                    .ok()
                    .and_then(|frame| frame.window.as_ref())
                    .map(|window| window.load),
                Some(nmp_engine::core::WindowLoad::Returned { .. })
            );
            if returned || frame.is_err() {
                batch_tx.send(frame).unwrap();
                break;
            }
        }
    });
    ready_rx.recv().unwrap();
    window_handle
        .request_rows(2)
        .expect("separate capability must grow the window while recv owns delivery");
    let frame = batch_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("growth must unblock the independently-owned receiver")
        .expect("window channel stays open");
    let contents = frame.window.expect("windowed frames carry window contents");
    assert_eq!(
        contents.load,
        nmp_engine::core::WindowLoad::Returned { added: 1 }
    );
    assert_eq!(contents.rows.len(), 2);
    drain.join().unwrap();

    // The drain's subscription has already dropped and cancelled the
    // shared session. A retained window-handle clone converges on that
    // same idempotent guard rather than issuing a second withdrawal.
    window_handle.cancel();
    engine.shutdown();
}

/// codex-nova's non-negotiable proof #3: an `Engine` with a LIVE query
/// subscription AND a live diagnostics subscription -- neither
/// cancelled, both still holding an outstanding `cancel_handle()` clone
/// nobody ever calls -- must still `shutdown()` cleanly within a
/// bounded wait. An outstanding, never-invoked cancel token must not
/// become a reason `shutdown` hangs or panics.
#[test]
fn shutdown_stays_clean_with_outstanding_cancel_tokens_for_query_and_diagnostics() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");

    let subscription = engine.observe(probe_query(), None).expect("engine is open");
    let diagnostics = engine.observe_diagnostics().expect("engine is open");

    // Obtain (but deliberately never call before shutdown) a cancel
    // token for each -- an outstanding, uninvoked token is the scenario
    // under test.
    let query_cancel = subscription.cancel_handle();
    let diagnostics_cancel = diagnostics.cancel_handle();

    subscription
        .recv()
        .expect("a fresh subscribe delivers one batch before anything else happens");
    diagnostics
        .recv()
        .expect("observe_diagnostics delivers the current snapshot immediately");

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect(
            "shutdown() must complete within a bounded wait even with outstanding, \
         never-cancelled tokens still alive",
        );

    // The outstanding tokens themselves must still be safe to cancel
    // (or simply drop) after the engine they named is already gone.
    query_cancel.cancel();
    diagnostics_cancel.cancel();
}

#[test]
fn live_nip11_cannot_outlive_real_engine_shutdown_with_retained_owners() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(std::sync::Barrier::new(2));
    let server_accepted = Arc::clone(&accepted);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        let mut buffer = [0u8; 1024];
        while !received.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "HTTP request ended before its headers");
            received.extend_from_slice(&buffer[..count]);
        }
        server_accepted.wait();
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    });

    // Issue #519: the resolved-IP admission check now refuses a loopback
    // dial by default, so this test's own `127.0.0.1` NIP-11 mock server
    // needs the same operator opt-in a real local relay would use.
    let engine = Arc::new(
        Engine::new(EngineConfig {
            ..EngineConfig::default()
        })
        .expect("engine must build"),
    );
    let retained_engine = Arc::clone(&engine);
    let subscription = engine.observe(probe_query(), None).expect("engine is open");
    subscription
        .recv()
        .expect("a fresh subscription delivers its initial frame");
    let cancel = subscription.cancel_handle();
    let relay = format!("ws://{address}");
    let acquisition = std::thread::spawn(move || {
        block_on(retained_engine.relay_information(&relay, RelayInformationCachePolicy::Refresh))
    });
    accepted.wait();

    let shutdown_engine = Arc::clone(&engine);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        shutdown_engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("live cancellable DNS/HTTP must not hold EngineThread shutdown");
    assert!(matches!(
        acquisition.join().unwrap(),
        Err(RelayInformationRequestError::Acquisition(
            RelayInformationError::ServiceClosed
        ))
    ));
    // #680 removed the native-task census surface; the real semantic here
    // is that shutdown drained the live acquisition (ServiceClosed above)
    // without blocking, and the subscription reaches disconnect. The
    // observation-evidence stream may still have its final pre-shutdown
    // batch queued after the initial row frame; consuming that bounded
    // batch before disconnect is delivery, not an outliving producer.
    let mut queued_after_shutdown = 0;
    loop {
        match subscription.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(_) => {
                queued_after_shutdown += 1;
                assert_eq!(
                    queued_after_shutdown, 1,
                    "the one-slot mailbox cannot retain multiple frames after shutdown"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("subscription producer outlived engine shutdown")
            }
        }
    }

    // These retained owners remain safe after exact-zero teardown.
    cancel.cancel();
    drop(subscription);
    drop(engine);
    server.join().unwrap();
}

#[test]
fn sixty_four_owned_facade_values_do_not_become_engine_retention() {
    const BODY_BYTES: usize = 256 * 1024;
    const CALLS: usize = 64;

    let prefix = r#"{"description":""#;
    let suffix = r#""}"#;
    let body = format!(
        "{prefix}{}{suffix}",
        "x".repeat(BODY_BYTES - prefix.len() - suffix.len())
    );
    assert_eq!(body.len(), BODY_BYTES);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        let mut buffer = [0u8; 1024];
        while !received.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "HTTP request ended before its headers");
            received.extend_from_slice(&buffer[..count]);
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    // Issue #519: opt the mock server's loopback host in — see the
    // identical note in `live_nip11_cannot_outlive_real_engine_shutdown_with_retained_owners`.
    let engine = Engine::new(EngineConfig {
        ..EngineConfig::default()
    })
    .expect("engine must build");
    let relay = format!("ws://{address}");
    let mut caller_owned = Vec::with_capacity(CALLS);
    caller_owned.push(
        block_on(engine.relay_information(&relay, RelayInformationCachePolicy::Refresh)).unwrap(),
    );
    server.join().unwrap();
    for _ in 1..CALLS {
        caller_owned.push(
            block_on(engine.relay_information(&relay, RelayInformationCachePolicy::UseCache))
                .unwrap(),
        );
    }
    assert!(caller_owned
        .iter()
        .all(|snapshot| snapshot.raw_json().len() == BODY_BYTES));

    let while_callers_retain = engine.relay_information_retention_census();
    assert_eq!(while_callers_retain.cached_entries, 1);
    assert_eq!(while_callers_retain.cached_payloads, 1);
    assert_eq!(while_callers_retain.cached_raw_body_bytes, BODY_BYTES);
    assert_eq!(while_callers_retain.active_flights, 0);
    assert_eq!(while_callers_retain.subscribed_callers, 0);

    // Caller-held snapshots share the cached payload. Dropping them cannot
    // change the engine census: the service retains its own cache entry.
    drop(caller_owned);
    assert_eq!(
        engine.relay_information_retention_census(),
        while_callers_retain
    );
    engine.shutdown();
}

fn probe_query() -> LiveQuery {
    LiveQuery::single(Demand {
        selection: nmp_grammar::Filter {
            // An arbitrary caller-owned kind, not any NIP-01 core schema --
            // see this module's other fixtures for why.
            kinds: Some(std::collections::BTreeSet::from([9999u16])),
            ..nmp_grammar::Filter::default()
        },
        ..Demand::default()
    })
}

fn window_probe() -> Window {
    Window::Expandable {
        initial: std::num::NonZeroUsize::new(1).unwrap(),
        max: std::num::NonZeroUsize::new(2).unwrap(),
    }
}

/// An unbounded observation has no window: `request_rows` is a typed
/// `Unwindowed` refusal and `window_handle()` is `None`. The growth
/// capability's very existence is derived from the window policy.
#[test]
fn unwindowed_observation_has_no_growth_capability() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let subscription = engine.observe(probe_query(), None).expect("engine is open");
    subscription
        .recv()
        .expect("a fresh subscribe delivers one batch");
    assert!(subscription.window_handle().is_none());
    assert_eq!(
        subscription.request_rows(10),
        Err(crate::RequestRowsError::Unwindowed)
    );
    engine.shutdown();
}

/// `initial > max` and a selection that already carries a NIP-01 `limit`
/// are typed `EngineError`s caught at `observe`, before the engine is
/// touched.
#[test]
fn windowed_observe_rejects_bad_bounds_and_competing_limit() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    assert_eq!(
        engine
            .observe(
                probe_query(),
                Some(Window::Expandable {
                    initial: std::num::NonZeroUsize::new(5).unwrap(),
                    max: std::num::NonZeroUsize::new(2).unwrap(),
                })
            )
            .err(),
        Some(EngineError::WindowInitialExceedsMax { initial: 5, max: 2 })
    );
    let mut branch = probe_query().branches()[0].clone();
    branch.selection.limit = Some(3);
    let limited = LiveQuery::single(branch);
    assert_eq!(
        engine.observe(limited, Some(window_probe())).err(),
        Some(EngineError::WindowSelectionHasLimit)
    );
    engine.shutdown();
}
