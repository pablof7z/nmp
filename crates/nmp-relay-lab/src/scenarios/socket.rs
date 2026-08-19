//! What a relay can do to the SOCKET and to the connection itself.
//!
//! Everything here is below NIP-01, which is why none of it was expressible
//! against a relay library: a half-written frame, injected octets, a
//! direction that goes quiet without closing, and an upgrade answered with a
//! login page are all things only the owner of the socket can do.

use std::time::Duration;

use nmp::{RelayInformationCachePolicy, RelayInformationFreshness};
use nostr::Keys;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::clock::PRODUCTION_RECONNECT_FLOOR;
use crate::fixtures::{engine_against, kind1_by, kind_by_on, notes, rows_within, QUIET, SETTLE};
use crate::scenario::Report;
use crate::{Direction, Nip11, RelayLab, Reply, Req, Script, Upgrade};

/// A frame truncated mid-payload. One event honestly, then a REAL `EVENT`
/// frame cut after twelve octets.
///
/// `keep_bytes` is the mutation handle, and it has to be: raise it to the
/// whole frame and the second event arrives as a row. That is what proves
/// this measures truncation rather than an event the client was never going
/// to accept anyway -- the shape the FIRST version of this got wrong, its
/// truncated frame naming a subscription the client had never opened.
pub async fn mid_frame_truncation(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("mid-frame-truncation");
    let keep = if mutation == Some("send-whole-frame") {
        usize::MAX
    } else {
        12
    };
    let author = Keys::generate();
    let corpus = notes(&author, 2, 1_700_000_000);
    let cut = corpus[1].clone();

    let relay = RelayLab::start(Script::new().seed(vec![corpus[0].clone()]).on_req(
        Req::any(),
        Reply::new().then_stored().then_partial_event(cut, keep),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    report.eq(
        "the event written in full before the cut is the app's; the cut one is not",
        rows.iter().map(nmp::Row::id).collect::<Vec<_>>(),
        vec![corpus[0].id],
    );
    report.that(
        "and the stored phase never terminated",
        relay.record().eosed_subscription_ids().is_empty(),
        relay.record().eosed_subscription_ids(),
    );
    report
}

/// A COMPLETE illegal frame fails the connection and NMP redials.
///
/// Complete and illegal: FIN set, reserved opcode `0x3`, zero length. That
/// distinction is the whole scenario. An INCOMPLETE header -- `[0xf3, 0x7f]`,
/// promising eight length bytes that never arrive -- provokes nothing at all:
/// the client waits forever and the connection stays up. Both are worth
/// saying and they are different faults; the incomplete one is
/// `mid-frame-truncation` above.
pub async fn illegal_frame(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("illegal-frame");
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
    report.that(
        format!("an illegal frame fails the connection and is redialled within {budget:?}"),
        relay.session_count() >= 2,
        relay.session_count(),
    );
    report.that(
        "and the record shows the octets the script actually wrote",
        relay
            .record()
            .frames
            .iter()
            .any(|frame| frame.payload == vec![0x83, 0x00]),
        relay.record().frames.len(),
    );
    report
}

/// A direction that stops writing WITHOUT closing. The socket stays open, the
/// relay says nothing, and the app waits.
pub async fn stalled_direction(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("stalled-direction");
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
    report.that("a stalled relay serves nothing", rows.is_empty(), rows.len());
    report.eq("the REQ did reach the relay", record.reqs().len(), 1);
    report.that(
        "and the relay wrote nothing back at all",
        record.frames.iter().all(|f| f.direction == Direction::Up),
        record.frames.len(),
    );
    report.eq(
        "the socket was never closed, so nothing was redialled",
        relay.session_count(),
        1,
    );
    report
}

/// Dropping the TCP connection mid-answer, with no websocket close.
pub async fn socket_drop(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("socket-drop");
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
    report.eq("the one event served before the drop is real", rows.len(), 1);
    report.that(
        "and no EOSE ever arrived",
        relay.record().eosed_subscription_ids().is_empty(),
        relay.record().eosed_subscription_ids(),
    );
    report
}

/// A captive portal: the upgrade answered with an HTTP 200 login page. Not a
/// refusal -- a success, for a completely different protocol.
pub async fn captive_portal(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("captive-portal");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().upgrade(Upgrade::Http {
        status: 200,
        content_type: "text/html".to_string(),
        body: "<html><body><h1>Sign in to use this network</h1></body></html>".to_string(),
    }))
    .await;

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
    let answer = String::from_utf8_lossy(&answer).to_string();
    report.that(
        "any client gets a 200 with a login page, not a refusal",
        answer.starts_with("HTTP/1.1 200") && answer.contains("Sign in to use this network"),
        answer.lines().next().unwrap_or("").to_string(),
    );

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(2));
    report.that("NMP is served nothing", rows.is_empty(), rows.len());
    report.that(
        "a portal never becomes a websocket, so no frame is decodable",
        relay.record().frames.is_empty(),
        relay.record().frames.len(),
    );
    report
}

/// One address, two protocols, exactly as a real relay does -- and a relay
/// that publishes nothing SAYS nothing (404).
pub async fn nip11_document(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("nip11-document");

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

    let advertised = RelayLab::start(Script::new().nip11(Nip11::limits(Some(3), Some(71)))).await;
    let response = fetch(advertised.port()).await;
    report.that(
        "a GET on the websocket address returns the NIP-11 document",
        response.starts_with("HTTP/1.1 200 OK")
            && response.contains("application/nostr+json")
            && response.contains("\"max_subscriptions\":3"),
        response.lines().next().unwrap_or("").to_string(),
    );

    // NMP's own acquisition path reads it, rather than the scenario injecting it.
    let engine = engine_against(&advertised);
    let snapshot = engine
        .relay_information(
            &advertised.url().to_string(),
            RelayInformationCachePolicy::Refresh,
        )
        .await
        .expect("the document is acquired over real HTTP");
    report.eq(
        "the advertised ceiling reaches the app through NMP's real fetcher",
        snapshot.document().limitation.max_subscriptions,
        Some(3),
    );
    report.eq(
        "a document just fetched is fresh",
        snapshot.freshness(),
        RelayInformationFreshness::Fresh,
    );

    let silent = RelayLab::start(Script::new()).await;
    let response = fetch(silent.port()).await;
    report.that(
        "a relay that publishes nothing answers 404",
        response.starts_with("HTTP/1.1 404"),
        response.lines().next().unwrap_or("").to_string(),
    );
    report
}

/// A subscription ceiling the relay never advertised.
///
/// The three observations name three different KINDS deliberately. NMP
/// collapses same-shape demands into one subscription with a unioned
/// `authors` list -- three same-kind queries reach this relay as a SINGLE
/// REQ, and a cap of two is then never approached. Correct behaviour, and
/// also why a scenario about a ceiling has to be written in demands that
/// cannot collapse.
pub async fn silent_subscription_cap(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("silent-subscription-cap");
    let cap = if mutation == Some("cap-of-nine") { 9 } else { 2 };
    let authors: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
    let relay = RelayLab::start(
        Script::new().cap_subscriptions(cap, "error: too many concurrent subscriptions"),
    )
    .await;

    let engine = engine_against(&relay);
    let _subscriptions: Vec<_> = authors
        .iter()
        .zip([1u16, 7, 30023])
        .map(|(author, kind)| {
            engine
                .observe(kind_by_on(author, relay.url(), kind), None)
                .expect("the observation opens locally, whatever the relay says")
        })
        .collect();

    let saw_refusal = relay
        .wire()
        .wait_for(Duration::from_secs(8), |record| {
            !record.closed_sent().is_empty()
        })
        .await;
    relay.wire().wait_quiet(QUIET, SETTLE).await;

    let record = relay.record();
    report.eq(
        "three uncollapsible demands are three REQs",
        record.reqs().len(),
        3,
    );
    report.that(
        "the excess REQ is refused, with the relay's own words",
        saw_refusal
            && record.closed_sent().len() == 1
            && record.closed_sent()[0].1 == "error: too many concurrent subscriptions",
        record.closed_sent(),
    );
    report.that(
        "no advance warning of any kind crossed the wire",
        !record.frames.iter().any(|f| f.verb() == Some("NOTICE")),
        record.frames.len(),
    );
    report
}

/// An ADVERTISED ceiling lower than what the app wants. What NMP does with a
/// limit it can read BEFORE it asks.
pub async fn advertised_subscription_cap(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("advertised-subscription-cap");
    let authors: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
    let relay = RelayLab::start(Script::new().nip11(Nip11::limits(Some(1), None))).await;

    let engine = engine_against(&relay);
    let snapshot = engine
        .relay_information(&relay.url().to_string(), RelayInformationCachePolicy::Refresh)
        .await
        .expect("the document is acquired");
    report.eq(
        "the ceiling really is advertised",
        snapshot.document().limitation.max_subscriptions,
        Some(1),
    );

    let _subscriptions: Vec<_> = authors
        .iter()
        .zip([1u16, 7, 30023])
        .map(|(author, kind)| {
            engine
                .observe(kind_by_on(author, relay.url(), kind), None)
                .expect("the observation opens")
        })
        .collect();

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq("all three demands became REQs", record.reqs().len(), 3);
    report.eq(
        "but only ONE is live at a time: NMP time-shares the slot rather than \
         exceeding what the relay said it would serve",
        record.live_subscription_ids().len(),
        1,
    );
    report.eq(
        "and NMP itself closed the other two -- the ceiling was honoured by \
         the CLIENT, since this relay enforces nothing",
        record.closes().len(),
        2,
    );
    report.that(
        "the relay refused nothing at all",
        record.closed_sent().is_empty(),
        record.closed_sent(),
    );
    report
}

/// Two engines, one relay. Each gets its own connection, session state and
/// slice of the record -- what a concurrent-edit scenario needs before it can
/// be written.
pub async fn two_engines(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("two-engines");
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

    report.eq(
        "the first engine sees both events",
        rows_within(&first_subscription, Duration::from_secs(3)).len(),
        2,
    );
    report.eq(
        "and so does the second",
        rows_within(&second_subscription, Duration::from_secs(3)).len(),
        2,
    );

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq(
        "two engines are two connections, never one interleaved stream",
        record.connections(),
        2,
    );
    let per_connection: Vec<usize> = (0..2)
        .map(|c| record.on_connection(c).reqs().len())
        .collect();
    report.that(
        "each connection carries its own REQ, and only its own",
        per_connection.iter().all(|&n| n == 1),
        &per_connection,
    );
    report
}

/// The relay goes away and comes back on the same address; NMP reconnects and
/// replays its subscription without the app asking again.
pub async fn reconnect_replay(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("reconnect-replay");
    let author = Keys::generate();
    let first = notes(&author, 1, 1_700_000_000);
    let relay = RelayLab::start(Script::new().seed(first.clone())).await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    report.eq(
        "the first event arrives before the relay goes away",
        rows_within(&subscription, Duration::from_secs(3)).len(),
        1,
    );

    let port = relay.disconnect().await;
    let later = crate::fixtures::note(&author, "published while the relay was away", 1_700_000_500);
    let relay = RelayLab::start_on_port(
        port,
        Script::new().seed(first.into_iter().chain([later.clone()])),
    )
    .await;

    let budget = PRODUCTION_RECONNECT_FLOOR * 3;
    report.that(
        format!("the subscription is replayed on the new connection within {budget:?}"),
        relay
            .wire()
            .wait_for(budget, |record| !record.reqs().is_empty())
            .await,
        relay.record().reqs().len(),
    );
    let rows = rows_within(&subscription, Duration::from_secs(5));
    report.that(
        "and the event seeded while it was away reaches the app",
        rows.iter().any(|row| row.id() == later.id),
        rows.len(),
    );
    report.eq(
        "this is the rebound instance's OWN record, starting at zero",
        relay.session_count(),
        1,
    );
    report
}
