//! What a relay can do to a READ, and what NMP observed when it did.
//!
//! Every count-shaped claim is checked against the relay's own record of the
//! octets, never against NMP's report of itself: a scenario that takes the
//! thing under test as its only witness passes when that thing is broken.

use std::time::Duration;

use nostr::Keys;

use crate::fixtures::{engine_against, kind1_by, note, notes, rows_within, QUIET, SETTLE};
use crate::scenario::Report;
use crate::{forge, RelayLab, Reply, Req, Script, Step};

pub const TRUNCATION_MUTATIONS: &[&str] = &["serve-41"];

/// Truncate silently: a hundred notes exist, the app asks for more than
/// forty, forty arrive, and EOSE says the relay has finished. Nothing NMP can
/// see distinguishes this from a relay that held exactly forty.
pub async fn truncation(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("truncation");
    let serve = if mutation == Some("serve-41") { 41 } else { 40 };

    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 100, 1_700_000_000))
            .on_req(Req::kind(1), Reply::truncate_at(serve)),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(4));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();

    report.eq(
        "the relay served exactly what the script said",
        record.served_event_ids().len(),
        40,
    );
    report.eq(
        "and told the app it had finished",
        record.eosed_subscription_ids().len(),
        1,
    );
    let asked = record.reqs()[0].max_limit();
    report.that(
        "the app asked for MORE than it was given",
        asked.unwrap_or(u64::MAX) > 40,
        asked,
    );
    report.eq("the app was shown forty rows, and no more", rows.len(), 40);
    report
}

/// Never EOSE: the REQ is accepted, every matching event streams, and the
/// stored phase is never terminated.
pub async fn never_eose(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("never-eose");
    let author = Keys::generate();
    let reply = if mutation == Some("send-eose") {
        Reply::stored()
    } else {
        Reply::never_eose()
    };
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 5, 1_700_000_000))
            .on_req(Req::any(), reply),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq("all five events were served", record.served_event_ids().len(), 5);
    report.that(
        "and the stored phase was NEVER terminated",
        record.eosed_subscription_ids().is_empty(),
        record.eosed_subscription_ids(),
    );
    report.eq("the app still saw every row", rows.len(), 5);
    report.that(
        "this is not the old harness's CLOSED approximation",
        record.closed_sent().is_empty(),
        record.closed_sent(),
    );
    report
}

/// EOSE, then more events on the same subscription. A client that treats EOSE
/// as the end of the stream loses these.
pub async fn eose_then_more(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("eose-then-more");
    let author = Keys::generate();
    let late = note(&author, "after the end of stored events", 1_700_000_500);
    let relay = RelayLab::start(Script::new().seed(notes(&author, 2, 1_700_000_000)).on_req(
        Req::kind(1),
        Reply::new()
            .then_stored()
            .then_eose()
            .after(Duration::from_millis(300))
            .then_events(vec![late.clone()]),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq("three events crossed the wire", record.served_event_ids().len(), 3);
    report.eq("one EOSE, in the middle", record.eosed_subscription_ids().len(), 1);
    report.that(
        "the POST-EOSE event still reaches the app",
        rows.iter().any(|row| row.id() == late.id),
        rows.len(),
    );
    report
}

/// CLOSED mid-subscription, with the relay's own words, after two of the five
/// events it holds.
pub async fn closed_midstream(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("closed-midstream");
    let author = Keys::generate();
    let corpus = notes(&author, 5, 1_700_000_000);
    let message = "rate-limited: slow down, you are asking too often";
    let relay = RelayLab::start(Script::new().seed(corpus.clone()).on_req(
        Req::kind(1),
        Reply::new()
            .then_events(corpus[3..].to_vec())
            .after(Duration::from_millis(200))
            .then_closed(message),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq("two events, then CLOSED", record.served_event_ids().len(), 2);
    report.eq(
        "the relay's exact sentence reached the wire",
        record.closed_sent(),
        vec![(record.reqs()[0].sub_id.clone(), message.to_string())],
    );
    report.eq("and the two events before it reached the app", rows.len(), 2);
    report
}

/// Serve events the client never asked for.
pub async fn filter_mismatch(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("filter-mismatch");
    let asked_about = Keys::generate();
    let stranger = Keys::generate();
    let intruder = note(&stranger, "nobody asked for this", 1_700_000_100);

    let relay = RelayLab::start(Script::new().seed(notes(&asked_about, 2, 1_700_000_000)).on_req(
        Req::kind(1),
        Reply::new()
            .then_stored()
            .then_events(vec![intruder.clone()])
            .then_eose(),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&asked_about, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.that(
        "the relay really did put the unasked-for event on the wire",
        record.served_event_ids().contains(&intruder.id.to_hex()),
        record.served_event_ids().len(),
    );
    report.that(
        "an event outside the query's own filter never becomes one of its rows",
        !rows.iter().any(|row| row.id() == intruder.id),
        rows.iter().map(nmp::Row::id).collect::<Vec<_>>(),
    );
    // The positive control, in the scenario rather than in a mutation someone
    // has to remember: an absence claim is worthless beside a pipeline that
    // delivered nothing at all.
    report.eq("the two events that DO match still arrived", rows.len(), 2);
    report
}

/// Two dishonest bodies and one honest one on the same subscription.
///
/// The honest event is the scenario's own positive control and is not
/// optional: "no rows arrived" beside a broken pipeline passes for the wrong
/// reason, and no mutation anyone has to remember would reveal it.
pub async fn dishonest_bodies(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("dishonest-bodies");
    let author = Keys::generate();
    let honest = note(&author, "what the author actually wrote", 1_700_000_000);
    let unsigned = note(&author, "never signed by this key", 1_700_000_001);
    let real = note(&author, "an ordinary, honest note", 1_700_000_002);

    let forged = if mutation == Some("serve-honestly") {
        serde_json::to_value(&honest).expect("an event renders as JSON")
    } else {
        forge::different_body_same_id(&honest, "not what the author wrote")
    };

    let relay = RelayLab::start(Script::new().on_req(
        Req::kind(1),
        Reply::new()
            .then_events_json(vec![forged, forge::bad_signature(&unsigned)])
            .then_events(vec![real.clone()])
            .then_eose(),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq(
        "all three bodies really were put on the wire",
        record.served_event_ids().len(),
        3,
    );
    report.eq(
        "exactly the honest event becomes a row, and neither dishonest one does",
        rows.iter().map(nmp::Row::id).collect::<Vec<_>>(),
        vec![real.id],
    );
    report
}

/// A NIP-42 challenge in the MIDDLE of a live subscription, not at connect.
pub async fn challenge_midstream(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("challenge-midstream");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().seed(notes(&author, 2, 1_700_000_000)).on_req(
        Req::kind(1),
        Reply::new()
            .then_stored()
            .then_eose()
            .after(Duration::from_millis(200))
            .then_auth("challenge-issued-mid-subscription"),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq(
        "the challenge was issued AFTER the stored phase, not at connect",
        record.auth_challenges(),
        vec!["challenge-issued-mid-subscription".to_string()],
    );
    report.eq("and the rows served before it are still the app's", rows.len(), 2);
    report
}

/// An unsolicited connect-time challenge is recorded and NOT answered.
///
/// Asserting the silence rather than an AUTH response is deliberate: the
/// first draft of this was named for the answer it expected and passed
/// without ever checking for one.
pub async fn challenge_at_connect(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("challenge-at-connect");
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 2, 1_700_000_000))
            .on_connect(Reply::auth("challenge-at-connect")),
    )
    .await;

    let engine = engine_against(&relay);
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("an account IS available, so silence is a choice and not a lack");
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq(
        "the challenge was issued before the client said anything",
        record.auth_challenges(),
        vec!["challenge-at-connect".to_string()],
    );
    report.that(
        "and NMP answered it with nothing -- a challenge nothing depends on \
         needs no identity",
        record.auth_responses().is_empty(),
        record.auth_responses().len(),
    );
    report.eq(
        "the unauthenticated read is served, because this relay gates nothing",
        rows.len(),
        2,
    );
    report
}

/// Rate-limiting mid-stream: part of what it holds, a NOTICE, and a stop --
/// without CLOSED, without EOSE.
pub async fn rate_limit_midstream(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("rate-limit-midstream");
    let author = Keys::generate();
    let corpus = notes(&author, 10, 1_700_000_000);
    let relay = RelayLab::start(Script::new().seed(corpus.clone()).on_req(
        Req::kind(1),
        Reply::new()
            .then(Step::Events(corpus[8..].to_vec()))
            .then_notice("rate-limited: too many concurrent requests"),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    report.eq("two of ten served", record.served_event_ids().len(), 2);
    report.that(
        "no EOSE followed the NOTICE",
        record.eosed_subscription_ids().is_empty(),
        record.eosed_subscription_ids(),
    );
    report.eq("the app kept the two", rows.len(), 2);
    report
}

/// Delay, per phase. The relay holds the answer and then serves it.
pub async fn delay(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("delay");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().seed(notes(&author, 3, 1_700_000_000)).on_req(
        Req::any(),
        Reply::new()
            .after(Duration::from_millis(700))
            .then_stored()
            .then_eose(),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");

    let early = rows_within(&subscription, Duration::from_millis(300));
    report.that(
        "nothing yet: the relay is still holding the answer",
        early.is_empty(),
        early.len(),
    );
    let rows = rows_within(&subscription, Duration::from_secs(3));
    report.eq("and then it answers in full", rows.len(), 3);

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    report.eq(
        "one EOSE, after the delay",
        relay.record().eosed_subscription_ids().len(),
        1,
    );
    report
}
