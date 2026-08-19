//! What a relay can do to a WRITE, and what NMP made of it.

use std::time::Duration;

use nmp::RelayState;
use nostr::Keys;

use crate::fixtures::{
    engine_against, kind1_by, note, notes, publish_note, publishing_engine, relay_facts,
    rows_within, RawSession, QUIET, SETTLE,
};
use crate::scenario::Report;
use crate::{Ev, RelayLab, Reply, Req, Script};

/// `OK: true`, and the relay keeps nothing. The one shape a receipt cannot
/// distinguish from a durable write, and the reason a receipt is a claim
/// about a relay's ANSWER rather than about the world.
pub async fn accepted_never_served(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("accepted-never-served");
    let author = Keys::generate();
    let reply = if mutation == Some("actually-store-it") {
        Reply::ok()
    } else {
        Reply::ok_but_forget()
    };
    let relay = RelayLab::start(Script::new().on_event(Ev::kind(1), reply)).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "acknowledged and forgotten");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    report.that(
        "the app was told the write landed",
        facts.contains(&RelayState::Published),
        &facts,
    );

    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let _ = rows_within(&subscription, Duration::from_secs(2));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq("exactly one acknowledgement", record.oks_sent().len(), 1);
    report.that("and it said true", record.oks_sent()[0].1, &record.oks_sent()[0]);
    report.that(
        "yet nothing was ever served back",
        record.served_event_ids().is_empty(),
        record.served_event_ids(),
    );
    report.that(
        "the relay kept nothing, which is what ok_but_forget means",
        relay.held().is_empty(),
        relay.held().len(),
    );
    report
}

/// The falsifier for the scenario above: the default write path is honest.
pub async fn stores_what_it_acknowledges(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("stores-what-it-acknowledges");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new()).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "acknowledged and kept");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    report.that("acknowledged", facts.contains(&RelayState::Published), &facts);
    report.eq(
        "an unscripted relay keeps what it acknowledges",
        relay.held().len(),
        1,
    );

    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));
    report.eq("and serves it back on a later read", rows.len(), 1);
    report
}

/// Every NIP-01 refusal prefix, mapped to the receipt fact NMP derives.
///
/// The table is the point. `classify_relay_ack` does NOT treat every `false`
/// as a refusal: `duplicate:` is an ACK (the relay HAS the event, which is
/// what the write wanted), `rate-limited:` and `error:` are transient and
/// retried, and only the genuine refusals terminate the lane. That taxonomy
/// was unexercisable end to end before this crate, because the old harness
/// could send exactly one refusal string with exactly one prefix.
pub async fn refusal_taxonomy(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("refusal-taxonomy");
    // (sentence, terminal here, means acked)
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
        let receipt = publish_note(&engine, "a write this relay answers with false");
        let facts = relay_facts(&receipt.statuses, Duration::from_secs(6));

        if acked {
            report.that(
                format!("{message:?} means the relay HAS it, so the lane is acked"),
                facts.contains(&RelayState::Published),
                &facts,
            );
        } else if terminal {
            report.that(
                format!("{message:?} reaches the receipt verbatim as a refusal"),
                facts.iter().any(|state| matches!(
                    state, RelayState::Rejected { reason } if reason == message)),
                &facts,
            );
        } else {
            report.that(
                format!("{message:?} is transient, so nothing terminal follows"),
                !facts.iter().any(|s| matches!(
                    s, RelayState::Published | RelayState::Rejected { .. })),
                &facts,
            );
        }

        relay.wire().wait_quiet(QUIET, SETTLE).await;
        let sent = relay.record().oks_sent();
        report.that(
            format!("the wire carried {message:?} verbatim, prefix included"),
            !sent.is_empty() && !sent[0].1 && sent[0].2 == message,
            sent.first().cloned(),
        );
    }
    report
}

/// `OK: true` WITH a message. An app that reads only the boolean throws the
/// sentence away.
pub async fn accepted_with_message(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("accepted-with-message");
    let author = Keys::generate();
    let message = "this event replaced an older one";
    let relay =
        RelayLab::start(Script::new().on_event(Ev::any(), Reply::ok_with(message))).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "accepted, with a footnote");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    report.that("published", facts.contains(&RelayState::Published), &facts);

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let sent = relay.record().oks_sent();
    report.that(
        "an accepted OK carrying a message crossed the wire",
        sent.len() == 1 && sent[0].1 && sent[0].2 == message,
        sent,
    );
    report
}

/// A relay may answer a write with silence. Nothing terminal ever arrives.
pub async fn unanswered_write(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("unanswered-write");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().on_event(Ev::any(), Reply::silence())).await;

    let engine = publishing_engine(&relay, &author);
    let receipt = publish_note(&engine, "a write nobody answers");
    let facts = relay_facts(&receipt.statuses, Duration::from_secs(3));
    report.that(
        "an unanswered write never resolves",
        !facts.iter().any(|s| matches!(
            s, RelayState::Published | RelayState::Rejected { .. })),
        &facts,
    );

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq(
        "NMP did put the event on the wire",
        record.published_event_ids().len(),
        1,
    );
    report.that(
        "and the relay said nothing back",
        record.oks_sent().is_empty(),
        record.oks_sent(),
    );
    report
}

/// Writes are verified BY DEFAULT -- id and schnorr -- before any rule runs.
/// The property that keeps every write scenario realistic, so it gets its own
/// falsifier rather than being assumed.
pub async fn write_verification(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("write-verification");
    let relay = RelayLab::start(Script::new()).await;
    let author = Keys::generate();
    let honest = note(&author, "signed", 1_700_000_000);
    let mut tampered = serde_json::to_value(&honest).expect("event renders as JSON");
    tampered["sig"] = serde_json::Value::String("11".repeat(64));

    // Spoken directly on the wire: NMP would never send this, which is
    // precisely why the relay's own refusal has to be proven here.
    let mut session = RawSession::connect(relay.port()).await;
    session
        .send(&serde_json::json!(["EVENT", tampered]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    let refused = replies.iter().any(|m| {
        let a = m.as_array().unwrap_or(&Vec::new()).clone();
        a.first().and_then(|v| v.as_str()) == Some("OK")
            && a.get(2) == Some(&serde_json::Value::Bool(false))
            && a.get(3).and_then(|v| v.as_str()).unwrap_or("").starts_with("invalid:")
    });
    report.that(
        "an unverifiable signature is refused with NIP-01's own prefix",
        refused,
        &replies,
    );
    report.that("and nothing is stored", relay.held().is_empty(), relay.held().len());

    // Opting out is per script, and visible in the scenario that does it.
    let lax = RelayLab::start(Script::new().accepts_unverified_writes()).await;
    let mut session = RawSession::connect(lax.port()).await;
    session
        .send(&serde_json::json!(["EVENT", tampered]).to_string())
        .await;
    let replies = session.read_messages(Duration::from_secs(2)).await;
    let accepted = replies.iter().any(|m| {
        m.as_array()
            .map(|a| a.get(2) == Some(&serde_json::Value::Bool(true)))
            .unwrap_or(false)
    });
    report.that("a script that opts out admits it", accepted, &replies);
    report.eq("and stores it", lax.held().len(), 1);
    report
}

/// `on_nth_req`: misbehave once, then behave. The rule that fires only on the
/// nth match is what makes a transient fault sayable without a state machine.
pub async fn transient_failure(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("transient-failure");
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 3, 1_700_000_000))
            .on_nth_req(1, Req::any(), Reply::closed("error: try again"))
            .on_req(Req::any(), Reply::stored()),
    )
    .await;

    let engine = engine_against(&relay);
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
    report.eq(
        "exactly the first REQ was refused -- the counter is relay-wide, so a \
         reconnect does not restart it",
        record.closed_sent().len(),
        1,
    );
    report.eq("and the second was answered honestly", rows.len(), 3);
    report
}
