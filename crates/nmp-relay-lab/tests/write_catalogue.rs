//! What a relay can do to a WRITE, and what NMP made of it.

mod support;

use std::time::Duration;

use nmp::RelayState;
use nmp_relay_lab::{Ev, RelayLab, Reply, Req, Script};
use nostr::Keys;
use support::{
    kind1_by, publish_note, publishing_engine, relay_facts, rows_within, QUIET, SETTLE,
};

/// `OK: true`, and the relay keeps nothing. The app is told its write landed;
/// a later read of the same query serves nothing at all. The one shape a
/// receipt cannot distinguish from a durable write, and the reason a receipt
/// is a claim about a relay's ANSWER rather than about the world.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_acknowledges_a_write_and_never_serves_it_back() {
    let author = Keys::generate();
    let relay =
        RelayLab::start(Script::new().on_event(Ev::kind(1), Reply::ok_but_forget())).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "acknowledged and forgotten");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    assert!(
        facts.contains(&RelayState::Published),
        "the app was told the write landed: {facts:?}"
    );

    // The same relay, asked for exactly that author's notes, holds nothing.
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let _ = rows_within(&subscription, Duration::from_secs(2));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(
        record.oks_sent().len(),
        1,
        "exactly one acknowledgement: {:?}",
        record.oks_sent()
    );
    assert!(record.oks_sent()[0].1, "and it said true");
    assert!(
        record.served_event_ids().is_empty(),
        "yet nothing was ever served back: {:?}",
        record.served_event_ids()
    );
    assert!(
        relay.held().is_empty(),
        "the relay kept nothing, which is what `ok_but_forget` means"
    );
}

/// The default write path is honest: the write is acknowledged AND stored, so
/// a read after it serves it back. The falsifier for the test above.
#[tokio::test(flavor = "multi_thread")]
async fn an_unscripted_relay_stores_what_it_acknowledges() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new()).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "acknowledged and kept");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    assert!(facts.contains(&RelayState::Published), "{facts:?}");

    assert_eq!(
        relay.held().len(),
        1,
        "an unscripted relay keeps what it acknowledges"
    );

    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));
    assert_eq!(rows.len(), 1, "and serves it back on a later read");
}

/// `OK: false` and NIP-01's machine-readable prefixes, each mapped to the
/// receipt fact NMP actually derives from it.
///
/// The table is the point. `classify_relay_ack`
/// (`crates/nmp-engine/src/core/mod.rs`) does NOT treat every `false` as a
/// refusal: `duplicate:` is an ACK (the relay has the event, which is what
/// the write wanted), `rate-limited:` and `error:` are transient and retried,
/// and only the genuine refusals terminate the lane. That taxonomy was
/// unexercisable end-to-end before this crate, because the old harness could
/// send exactly one refusal string with exactly one prefix.
#[tokio::test(flavor = "multi_thread")]
async fn each_refusal_prefix_reaches_the_receipt_as_the_fact_it_means() {
    // (relay's exact sentence, is it terminal here, does it mean acked)
    let table = [
        ("duplicate: we already have this event", true, true),
        ("blocked: this pubkey is not admitted here", true, false),
        ("invalid: this kind is not accepted", true, false),
        ("pow: 24 bits of difficulty required", true, false),
        ("restricted: you are not a member of this group", true, false),
        ("rate-limited: slow down, you are publishing too fast", false, false),
        ("error: something went wrong on our side", false, false),
    ];

    for (message, terminal, acked) in table {
        let author = Keys::generate();
        let relay =
            RelayLab::start(Script::new().on_event(Ev::any(), Reply::rejected(message))).await;
        let engine = publishing_engine(&relay, &author);
        let receipt = publish_note(&engine, "a write this relay will answer with false");
        let facts = relay_facts(&receipt.statuses, Duration::from_secs(6));

        if acked {
            assert!(
                facts.contains(&RelayState::Published),
                "{message:?} means the relay HAS the event, so the lane is acked; got {facts:?}"
            );
        } else if terminal {
            assert!(
                facts.iter().any(|state| matches!(
                    state,
                    RelayState::Rejected { reason } if reason == message
                )),
                "{message:?} must reach the receipt as a refusal carrying the relay's own \
                 sentence, verbatim; got {facts:?}"
            );
        } else {
            assert!(
                !facts.iter().any(|state| matches!(
                    state,
                    RelayState::Published | RelayState::Rejected { .. }
                )),
                "{message:?} is transient, so nothing terminal may follow it; got {facts:?}"
            );
        }

        relay.wire().wait_quiet(QUIET, SETTLE).await;
        let sent = relay.record().oks_sent();
        assert!(!sent.is_empty(), "the relay answered {message:?}");
        assert!(!sent[0].1, "and the wire really said false for {message:?}");
        assert_eq!(sent[0].2, message, "verbatim, prefix included");
    }
}

/// `OK: true` WITH a message. Both halves are real: NIP-01 allows a relay to
/// say something alongside an acceptance, and an app that only reads the
/// boolean throws the sentence away.
#[tokio::test(flavor = "multi_thread")]
async fn an_acceptance_may_carry_a_message_too() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new().on_event(Ev::any(), Reply::ok_with("this event replaced an older one")),
    )
    .await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "accepted, with a footnote");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    assert!(facts.contains(&RelayState::Published), "{facts:?}");

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let sent = relay.record().oks_sent();
    assert_eq!(
        sent,
        vec![(
            sent[0].0.clone(),
            true,
            "this event replaced an older one".to_string()
        )],
        "the relay put an accepted OK with a message on the wire"
    );
}

/// A relay may answer a write with silence. Nothing terminal ever arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_may_never_answer_a_write_at_all() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().on_event(Ev::any(), Reply::silence())).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "a write nobody answers");
    let facts = relay_facts(&receipt.statuses, Duration::from_secs(3));

    assert!(
        !facts.iter().any(|state| matches!(
            state,
            RelayState::Published | RelayState::Rejected { .. }
        )),
        "an unanswered write must never resolve: {facts:?}"
    );

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(
        record.published_event_ids().len(),
        1,
        "NMP did put the event on the wire"
    );
    assert!(
        record.oks_sent().is_empty(),
        "and the relay said nothing back"
    );
}

/// Writes are verified BY DEFAULT: id and schnorr signature, before any rule
/// runs. This is the property that keeps every scenario above realistic, so
/// it gets its own falsifier rather than being assumed.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_whose_signature_does_not_verify_is_refused_by_default() {
    let relay = RelayLab::start(Script::new()).await;
    let author = Keys::generate();
    let honest = support::note(&author, "signed", 1_700_000_000);
    let mut tampered = serde_json::to_value(&honest).expect("event renders as JSON");
    tampered["sig"] = serde_json::Value::String("11".repeat(64));

    // Spoken directly on the wire: NMP itself would never send this, which is
    // precisely why the relay's own refusal has to be proven here.
    let sent = raw_event(relay.port(), &tampered).await;
    assert!(
        sent.contains("\"OK\"") && sent.contains("false") && sent.contains("invalid:"),
        "an unverifiable signature must be refused with NIP-01's own prefix; got {sent}"
    );
    assert!(
        relay.held().is_empty(),
        "and nothing may be stored: {:?}",
        relay.held().len()
    );

    // Opting out is per script, and visible in the scenario that does it.
    let lax = RelayLab::start(Script::new().accepts_unverified_writes()).await;
    let sent = raw_event(lax.port(), &tampered).await;
    assert!(
        sent.contains("true"),
        "a script that opts out admits it; got {sent}"
    );
    assert_eq!(lax.held().len(), 1);
}

/// Speak one EVENT frame on a raw socket and return the relay's reply.
async fn raw_event(port: u16, body: &serde_json::Value) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the relay accepts a plain TCP client");
    socket
        .write_all(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .expect("the upgrade is writable");
    let mut head = [0u8; 512];
    let _ = socket.read(&mut head).await.expect("handshake reply");

    let payload = serde_json::json!(["EVENT", body]).to_string();
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let bytes = payload.as_bytes();
    let mut frame = vec![0x81];
    if bytes.len() < 126 {
        frame.push(0x80 | bytes.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    socket.write_all(&frame).await.expect("the frame is writable");

    let mut reply = [0u8; 2048];
    let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut reply))
        .await
        .expect("the relay answers within the bound")
        .expect("the reply is readable");
    String::from_utf8_lossy(&reply[..read]).to_string()
}

/// `on_nth_req`: misbehave once, then behave. The rule that fires only on the
/// first match is what makes a transient fault sayable without the scenario
/// building a state machine.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_can_fail_exactly_once_and_then_answer_honestly() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(support::notes(&author, 3, 1_700_000_000))
            .on_nth_req(1, Req::any(), Reply::closed("error: try again"))
            .on_req(Req::any(), Reply::stored()),
    )
    .await;

    let engine = support::engine_against(&relay);
    let first = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let _ = rows_within(&first, Duration::from_secs(1));
    drop(first);

    let second = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("a second observation opens");
    let rows = rows_within(&second, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(
        record.closed_sent().len(),
        1,
        "exactly the first REQ was refused: {:?}",
        record.closed_sent()
    );
    assert_eq!(rows.len(), 3, "and the second was answered honestly");
}
