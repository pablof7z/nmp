//! What a relay can do to the SOCKET and to the connection itself.
//!
//! Everything here is below NIP-01, which is why none of it was expressible
//! against a relay library: a half-written frame, injected octets, a
//! direction that goes quiet without closing, and an upgrade answered with a
//! login page are all things only the owner of the socket can do.

mod support;

use std::time::Duration;

use nmp::{RelayInformationCachePolicy, RelayInformationFreshness};
use nmp_relay_lab::{clock::PRODUCTION_RECONNECT_FLOOR, Nip11, RelayLab, Reply, Req, Script, Upgrade};
use nostr::Keys;
use support::{engine_against, kind1_by, notes, rows_within, QUIET, SETTLE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A frame truncated mid-payload. The relay serves one event honestly, then
/// writes a REAL `EVENT` frame for a second one and cuts it after twelve
/// octets. The client is left holding a header promising bytes that never
/// arrive: the event before the cut is the app's, the cut one is not, and
/// nothing after it can ever be parsed.
///
/// `keep_bytes` is the mutation handle, and it has to be: raise it to the
/// whole frame and the second event arrives as a row, which is what proves
/// this test is measuring truncation rather than an event the client was
/// never going to accept anyway.
#[tokio::test(flavor = "multi_thread")]
async fn a_frame_truncated_mid_payload_ends_the_stream_where_it_was_cut() {
    let author = Keys::generate();
    let corpus = notes(&author, 2, 1_700_000_000);
    let cut = corpus[1].clone();

    let relay = RelayLab::start(
        Script::new()
            .seed(vec![corpus[0].clone()])
            .on_req(
                Req::any(),
                Reply::new().then_stored().then_partial_event(cut.clone(), 12),
            ),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    assert_eq!(
        rows.iter().map(nmp::Row::id).collect::<Vec<_>>(),
        vec![corpus[0].id],
        "the event written in full before the cut is the app's; the cut one is not"
    );
    assert!(
        relay.record().eosed_subscription_ids().is_empty(),
        "and the stored phase never terminated"
    );
}

/// Octets that are not a legal websocket frame, injected mid-stream. NMP's
/// client cannot parse them, so it fails the connection -- and redials.
///
/// The frame is COMPLETE and illegal: FIN set, reserved opcode `0x3`, zero
/// length. That distinction is the whole test. An INCOMPLETE frame header --
/// `[0xf3, 0x7f, ...]`, whose `0x7f` promises eight more length bytes that
/// never arrive -- provokes nothing at all: the client simply waits for the
/// rest, forever, and the connection stays up. Both are worth being able to
/// say, and they are different faults with different consequences; the
/// incomplete one is `Step::PartialFrame`, tested above.
///
/// The redial is slow on purpose: the wait is [`PRODUCTION_RECONNECT_FLOOR`],
/// which a scenario cannot shorten today (see `nmp_relay_lab::clock`). That
/// cost is a finding, not a property of this test.
#[tokio::test(flavor = "multi_thread")]
async fn an_illegal_frame_fails_the_connection_and_nmp_redials() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().seed(notes(&author, 2, 1_700_000_000)).on_req(
        Req::any(),
        Reply::new()
            .then_stored()
            .then_eose()
            .after(Duration::from_millis(100))
            .then_bytes(vec![0x83, 0x00]),
    ))
    .await;

    let engine = engine_against(&relay);
    let _subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");

    let budget = PRODUCTION_RECONNECT_FLOOR * 3;
    let deadline = std::time::Instant::now() + budget;
    while relay.session_count() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        relay.session_count() >= 2,
        "an illegal frame must fail the connection and be redialled within {budget:?}; \
         websocket sessions seen: {}",
        relay.session_count()
    );
    assert!(
        relay
            .record()
            .frames
            .iter()
            .any(|frame| frame.payload == vec![0x83, 0x00]),
        "and the record must show the octets the script actually wrote"
    );
}

/// A direction that stops writing WITHOUT closing. The socket stays open, the
/// relay says nothing more, and the app waits. This is the shape a client
/// cannot distinguish from a slow relay, and the one a keepalive exists for.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_direction_holds_the_socket_open_and_says_nothing() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 3, 1_700_000_000))
            .on_req(Req::any(), Reply::new().then_stall()),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(2));

    let record = relay.record();
    assert!(rows.is_empty(), "a stalled relay serves nothing");
    assert_eq!(record.reqs().len(), 1, "the REQ did reach the relay");
    assert!(
        record.frames.iter().all(|f| f.direction == nmp_relay_lab::Direction::Up),
        "and the relay wrote nothing back at all"
    );
    assert_eq!(
        relay.session_count(),
        1,
        "the socket was never closed, so nothing was ever redialled"
    );
}

/// Dropping the TCP connection mid-answer, with no websocket close.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_may_drop_the_socket_partway_through_an_answer() {
    let author = Keys::generate();
    let corpus = notes(&author, 4, 1_700_000_000);
    let relay = RelayLab::start(Script::new().seed(corpus.clone()).on_req(
        Req::any(),
        Reply::new()
            .then_events(corpus[3..].to_vec())
            .after(Duration::from_millis(100))
            .then_disconnect(),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(2));

    assert_eq!(rows.len(), 1, "the one event served before the drop is real");
    assert!(relay.record().eosed_subscription_ids().is_empty());
}

/// A captive portal: the upgrade is answered with an HTTP 200 login page.
/// Not a refusal -- a success, for a completely different protocol. A client
/// that only handles a clean 4xx gets this wrong.
#[tokio::test(flavor = "multi_thread")]
async fn a_captive_portal_answers_the_upgrade_with_a_login_page() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().upgrade(Upgrade::Http {
        status: 200,
        content_type: "text/html".to_string(),
        body: "<html><body><h1>Sign in to use this network</h1></body></html>".to_string(),
    }))
    .await;

    // The portal really is what any client would get.
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", relay.port()))
        .await
        .expect("the address accepts a client");
    socket
        .write_all(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .expect("the upgrade is writable");
    let mut answer = Vec::new();
    socket.read_to_end(&mut answer).await.expect("the portal answers");
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("Sign in to use this network"), "{answer}");

    // And NMP gets exactly that, so no NIP-01 frame ever crosses.
    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    assert!(rows_within(&subscription, Duration::from_secs(2)).is_empty());
    assert!(
        relay.record().frames.is_empty(),
        "a portal never becomes a websocket, so no frame is decodable: {:?}",
        relay.record().frames.len()
    );
}

/// One address, two protocols -- exactly as a real relay does. A `GET` gets
/// the NIP-11 document; the same address still accepts websockets. And a
/// relay that publishes nothing SAYS nothing: 404, which is what public
/// relays that advertise no document actually return.
#[tokio::test(flavor = "multi_thread")]
async fn one_address_serves_the_nip11_document_and_the_websocket() {
    async fn fetch(port: u16) -> String {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the address accepts a plain client");
        socket
            .write_all(
                b"GET / HTTP/1.1\r\nHost: localhost\r\n\
                  Accept: application/nostr+json\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("the request is writable");
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.expect("it answers");
        String::from_utf8_lossy(&response).to_string()
    }

    let advertised =
        RelayLab::start(Script::new().nip11(Nip11::limits(Some(3), Some(71)))).await;
    let response = fetch(advertised.port()).await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("application/nostr+json"), "{response}");
    assert!(response.contains("\"max_subscriptions\":3"), "{response}");

    // NMP's own acquisition path reads it, rather than the test injecting it.
    let engine = engine_against(&advertised);
    let snapshot = engine
        .relay_information(
            &advertised.url().to_string(),
            RelayInformationCachePolicy::Refresh,
        )
        .await
        .expect("the document is acquired over real HTTP");
    assert_eq!(
        snapshot.document().limitation.max_subscriptions,
        Some(3),
        "the advertised ceiling reaches the app: {:?}",
        snapshot.document()
    );
    assert_eq!(
        snapshot.freshness(),
        RelayInformationFreshness::Fresh,
        "a document just fetched over real HTTP is fresh"
    );

    let silent = RelayLab::start(Script::new()).await;
    let response = fetch(silent.port()).await;
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "a relay that publishes nothing must SAY nothing: {response}"
    );
}

/// A subscription ceiling the relay never advertised. The document says
/// nothing; the third REQ is simply CLOSED. A client cannot see this coming.
///
/// The three observations name three different KINDS deliberately. NMP
/// collapses same-shape demands into one subscription with a unioned
/// `authors` list -- three same-kind queries reach this relay as a single
/// REQ, and a cap of two is then never approached. That is correct
/// behaviour and it is also why a scenario about a subscription ceiling has
/// to be written in demands that cannot collapse.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_caps_subscriptions_without_advertising_the_cap() {
    let authors: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
    let relay = RelayLab::start(
        Script::new().cap_subscriptions(2, "error: too many concurrent subscriptions"),
    )
    .await;

    let engine = engine_against(&relay);
    // Held for the whole scenario: a dropped subscription frees its slot.
    let _subscriptions: Vec<_> = authors
        .iter()
        .zip([1u16, 7, 30023])
        .map(|(author, kind)| {
            engine
                .observe(support::kind_by_on(author, &relay, kind), None)
                .expect("the observation opens locally, whatever the relay says")
        })
        .collect();

    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| !record.closed_sent().is_empty())
            .await,
        "the relay must refuse the excess REQ"
    );
    relay.wire().wait_quiet(QUIET, SETTLE).await;

    let record = relay.record();
    assert_eq!(
        record.reqs().len(),
        3,
        "three uncollapsible demands are three REQs: {:?}",
        record.subscription_ids()
    );
    let closed = record.closed_sent();
    assert_eq!(closed.len(), 1, "exactly the third was refused: {closed:?}");
    assert_eq!(
        closed[0].1, "error: too many concurrent subscriptions",
        "with the relay's own words"
    );
    // Nothing warned about it: this relay publishes no document at all, so
    // the cap was invisible until the REQ was already refused.
    assert!(
        !record
            .frames
            .iter()
            .any(|frame| frame.verb() == Some("NOTICE")),
        "no advance warning of any kind crossed the wire"
    );
}

/// Two engines, one relay. Each gets its own connection, its own session
/// state, and its own slice of the record -- which is what a concurrent-edit
/// scenario needs before it can even be written.
#[tokio::test(flavor = "multi_thread")]
async fn two_engines_share_one_relay_without_sharing_a_connection() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().seed(notes(&author, 2, 1_700_000_000))).await;

    let first = engine_against(&relay);
    let second = engine_against(&relay);
    let first_subscription = first
        .observe(kind1_by(&author, &relay), None)
        .expect("the first engine observes");
    let second_subscription = second
        .observe(kind1_by(&author, &relay), None)
        .expect("the second engine observes");

    assert_eq!(rows_within(&first_subscription, Duration::from_secs(3)).len(), 2);
    assert_eq!(rows_within(&second_subscription, Duration::from_secs(3)).len(), 2);

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(
        record.connections(),
        2,
        "two engines are two connections, never one interleaved stream"
    );
    for connection in 0..2 {
        assert_eq!(
            record.on_connection(connection).reqs().len(),
            1,
            "each connection carries its own REQ, and only its own"
        );
    }
}

/// An ADVERTISED subscription ceiling lower than what the app wants.
///
/// The relay publishes `max_subscriptions: 1` and enforces nothing. What NMP
/// does with a limit it can read before it asks is the question; this
/// scenario reads the answer off the wire rather than off diagnostics.
#[tokio::test(flavor = "multi_thread")]
async fn an_advertised_subscription_ceiling_is_visible_before_the_first_req() {
    let authors: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
    let relay = RelayLab::start(Script::new().nip11(Nip11::limits(Some(1), None))).await;

    let engine = engine_against(&relay);
    let snapshot = engine
        .relay_information(
            &relay.url().to_string(),
            RelayInformationCachePolicy::Refresh,
        )
        .await
        .expect("the document is acquired");
    assert_eq!(
        snapshot.document().limitation.max_subscriptions,
        Some(1),
        "the ceiling really is advertised"
    );

    let _subscriptions: Vec<_> = authors
        .iter()
        .zip([1u16, 7, 30023])
        .map(|(author, kind)| {
            engine
                .observe(support::kind_by_on(author, &relay, kind), None)
                .expect("the observation opens")
        })
        .collect();

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(
        record.reqs().len(),
        3,
        "all three demands were compiled into REQs: {:?}",
        record.subscription_ids()
    );
    assert_eq!(
        record.live_subscription_ids().len(),
        1,
        "but only ONE is live at a time against an advertised ceiling of one: \
         NMP time-shares the slot rather than exceeding what the relay said it \
         would serve; live: {:?}, closes: {:?}",
        record.live_subscription_ids(),
        record.closes()
    );
    assert_eq!(
        record.closes().len(),
        2,
        "and NMP itself closed the other two -- the ceiling was honoured by \
         the CLIENT, since this relay enforces nothing: {:?}",
        record.closes()
    );
    assert!(
        record.closed_sent().is_empty(),
        "the relay refused nothing at all: {:?}",
        record.closed_sent()
    );
}

/// The relay goes away and comes back on the same address. NMP reconnects on
/// its own and replays the subscription it had open, so the events seeded
/// while it was gone arrive without the app asking again.
///
/// The rebound relay is a NEW instance with its OWN record starting at zero,
/// which is what makes "the client came back" a deterministic witness rather
/// than a guess at how long a reconnect takes. The wait is still real time:
/// see `nmp_relay_lab::clock` for why it cannot be jumped.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_comes_back_gets_its_subscription_replayed() {
    let author = Keys::generate();
    let first = notes(&author, 1, 1_700_000_000);
    let relay = RelayLab::start(Script::new().seed(first.clone())).await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    assert_eq!(
        rows_within(&subscription, Duration::from_secs(3)).len(),
        1,
        "the first event arrives before the relay goes away"
    );

    // Gone: the listener is released and every socket severed before this
    // returns, so the port is free to rebind.
    let port = relay.disconnect().await;

    // Back, on the same address, now holding something new.
    let later = support::note(&author, "published while the relay was away", 1_700_000_500);
    let relay = RelayLab::start_on_port(
        port,
        Script::new().seed(first.into_iter().chain([later.clone()])),
    )
    .await;
    assert!(
        nmp_relay_lab::wait_reachable(
            format!("127.0.0.1:{port}").parse().expect("loopback address"),
            Duration::from_secs(2)
        )
        .await,
        "the rebound relay is listening again"
    );

    // NMP reconnects and replays on its own; the app asked for nothing.
    let budget = PRODUCTION_RECONNECT_FLOOR * 3;
    assert!(
        relay
            .wire()
            .wait_for(budget, |record| !record.reqs().is_empty())
            .await,
        "the subscription must be replayed on the new connection within {budget:?}"
    );

    let rows = rows_within(&subscription, Duration::from_secs(5));
    assert!(
        rows.iter().any(|row| row.id() == later.id),
        "and the event seeded while the relay was away reaches the app: {} rows",
        rows.len()
    );
    assert_eq!(
        relay.session_count(),
        1,
        "this is the rebound instance's OWN record, starting at zero"
    );
}
