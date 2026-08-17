//! #1369 — an ordinary query must distinguish an accepted unsigned row from
//! a cryptographically signed event, and it must learn promotion without
//! reopening or receiving a duplicate row.

use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Demand, Engine, EngineConfig, Filter, Identity, LiveQuery, RowDelta, RowSignature,
    SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability,
    WriteIntent, WritePayload, WriteRouting,
};
use nmp_grammar::EventBuilder;
use nmp_signer::PendingSignerSender;
use nostr::{Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

struct HeldSigner {
    pubkey: PublicKey,
    started: mpsc::Sender<(SignerUnsignedEvent, PendingSignerSender<SignerSignedEvent>)>,
}

impl SigningCapability for HeldSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new(self.pubkey.to_bytes()))
    }

    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        let (sender, operation) = SignerOp::pending_channel();
        self.started
            .send((unsigned, sender))
            .expect("the test owns the signer request receiver");
        operation
    }
}

fn to_nostr_unsigned(unsigned: SignerUnsignedEvent) -> UnsignedEvent {
    let (public_key, created_at, kind, tags, content) = unsigned.into_parts();
    UnsignedEvent::new(
        PublicKey::from_slice(public_key.as_bytes()).expect("the engine supplied a public key"),
        Timestamp::from(created_at),
        Kind::from(kind),
        tags.into_iter()
            .map(Tag::parse)
            .collect::<Result<Vec<_>, _>>()
            .expect("the engine supplied valid tags"),
        content,
    )
}

fn to_signer_event(event: nostr::Event) -> SignerSignedEvent {
    SignerSignedEvent::new(
        event.id.to_bytes(),
        SignerPublicKey::new(event.pubkey.to_bytes()),
        event.created_at.as_secs(),
        event.kind.as_u16(),
        event.tags.into_iter().map(|tag| tag.to_vec()).collect(),
        event.content,
        event.sig.serialize(),
    )
}

#[test]
fn delayed_signer_promotes_the_same_visible_row_from_pending_to_signed() {
    let keys = Keys::generate();
    let pubkey = keys.public_key();
    let (started_tx, started_rx) = mpsc::channel();
    let engine = Engine::new(EngineConfig::default()).expect("the temporary Redb engine starts");
    engine
        .add_public_key_account(pubkey, false)
        .expect("the session account exists");
    engine
        .install_test_signing_capability(HeldSigner {
            pubkey,
            started: started_tx,
        })
        .expect("the held signer has an explicit public key");

    let subscription = engine
        .observe(
            LiveQuery::single(
                Demand::author_outboxes(Filter {
                    kinds: Some(BTreeSet::from([1])),
                    authors: Some(Binding::Literal(BTreeSet::from([pubkey.to_hex()]))),
                    ..Filter::default()
                })
                .expect("the selection binds `authors`"),
            ),
            None,
        )
        .expect("the ordinary query opens before publication");

    let _receipt = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder {
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "visible before the remote signer answers".to_string(),
                created_at: Some(Timestamp::from(1_800_000_000)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(pubkey),
        })
        .expect("the unsigned write is durably accepted");

    let (unsigned, completion) = started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the engine hands the exact frozen body to the signer");
    let signed = to_nostr_unsigned(unsigned)
        .sign_with_keys(&keys)
        .expect("the exact key signs the frozen body");
    let expected_id = signed.id;

    let deadline = Instant::now() + Duration::from_secs(5);
    let pending = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the optimistic pending row never arrived"
        );
        let frame = subscription
            .recv_timeout(remaining)
            .expect("the query stays open while the signer is pending");
        if let Some(row) = frame.deltas.into_iter().find_map(|delta| match delta {
            RowDelta::Added(row) if row.id() == expected_id => Some(row),
            _ => None,
        }) {
            break row;
        }
    };
    assert_eq!(pending.signature(), RowSignature::Pending);
    assert_eq!(pending.id(), expected_id);
    assert!(
        pending.signed_event().is_none(),
        "a pending app row must not expose NMP's internal storage sentinel"
    );

    completion
        .resolve(Ok(to_signer_event(signed.clone())))
        .expect("the first signer result owns the completion door");

    let deadline = Instant::now() + Duration::from_secs(5);
    let promoted = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the same-id signed update never arrived"
        );
        let frame = subscription
            .recv_timeout(remaining)
            .expect("the query stays open through signature promotion");
        if let Some(row) = frame.deltas.into_iter().find_map(|delta| match delta {
            RowDelta::Updated(row) if row.id() == expected_id => Some(row),
            RowDelta::Added(row) if row.id() == expected_id => {
                panic!("promotion must update the existing row, not add it twice: {row:?}")
            }
            _ => None,
        }) {
            break row;
        }
    };
    assert_eq!(promoted.signature(), RowSignature::Signed(signed.sig));
    assert_eq!(promoted.id(), pending.id());
    promoted
        .signed_event()
        .expect("a signed row always carries signature bytes")
        .verify()
        .expect("the promoted row carries the signer's verified signature");

    drop(subscription);
    engine.shutdown();
}
