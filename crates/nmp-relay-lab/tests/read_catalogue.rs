//! What a relay can do to a READ, and what NMP observed when it did.
//!
//! Every count-shaped claim is checked against the relay's own record of the
//! octets, never against NMP's report of itself: a scenario that takes the
//! thing under test as its only witness passes when that thing is broken.

mod support;

use std::time::Duration;

use nmp_relay_lab::{forge, RelayLab, Reply, Req, Script, Step};
use nostr::Keys;
use support::{engine_against, kind1_by, note, notes, rows_within, QUIET, SETTLE};

/// Truncate silently: a hundred notes exist, the app asks for more than
/// forty, forty arrive, and EOSE says the relay has finished. Nothing NMP can
/// see distinguishes this from a relay that held exactly forty.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_serves_forty_of_a_hundred_and_says_it_has_finished() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 100, 1_700_000_000))
            .on_req(Req::kind(1), Reply::truncate_at(40)),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(4));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();

    assert_eq!(
        record.served_event_ids().len(),
        40,
        "the relay served exactly what the script said"
    );
    assert_eq!(
        record.eosed_subscription_ids().len(),
        1,
        "and told the app it had finished"
    );
    assert!(
        record.reqs()[0].max_limit().unwrap_or(u64::MAX) > 40,
        "the app asked for more than it was given: {:?}",
        record.reqs()[0].filters
    );
    assert_eq!(rows.len(), 40, "the app was shown forty rows, and no more");
}

/// Never EOSE: the REQ is accepted, every matching event streams, and the
/// stored phase is never terminated. The subscription stays open forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_streams_everything_and_never_confirms_it_finished() {
    let author = Keys::generate();
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 5, 1_700_000_000))
            .on_req(Req::any(), Reply::never_eose()),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(record.served_event_ids().len(), 5, "all five were served");
    assert!(
        record.eosed_subscription_ids().is_empty(),
        "and the stored phase was never terminated"
    );
    assert_eq!(rows.len(), 5, "the app still saw every row");
    assert!(
        !record.closed_sent().iter().any(|(_, m)| !m.is_empty()),
        "this is NOT the old harness's CLOSED approximation: {:?}",
        record.closed_sent()
    );
}

/// EOSE, then more events on the same subscription. A client that treats EOSE
/// as the end of the stream loses these.
#[tokio::test(flavor = "multi_thread")]
async fn events_arriving_after_eose_still_reach_the_app() {
    let author = Keys::generate();
    let late = note(&author, "after the end of stored events", 1_700_000_500);
    let relay = RelayLab::start(
        Script::new()
            .seed(notes(&author, 2, 1_700_000_000))
            .on_req(
                Req::kind(1),
                Reply::new()
                    .then_stored()
                    .then_eose()
                    .after(Duration::from_millis(300))
                    .then_events(vec![late.clone()]),
            ),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(record.served_event_ids().len(), 3);
    assert_eq!(record.eosed_subscription_ids().len(), 1);
    assert!(
        rows.iter().any(|row| row.id() == late.id),
        "the post-EOSE event must reach the app; got {} rows",
        rows.len()
    );
}

/// CLOSED mid-subscription, with the relay's own words, after two of the five
/// events it holds. The message is the relay's, verbatim, because that is the
/// string an app would show a person.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_closes_a_live_subscription_partway_through_with_its_own_words() {
    let author = Keys::generate();
    let corpus = notes(&author, 5, 1_700_000_000);
    let relay = RelayLab::start(Script::new().seed(corpus.clone()).on_req(
        Req::kind(1),
        Reply::new()
            .then_events(corpus[3..].to_vec())
            .after(Duration::from_millis(200))
            .then_closed("rate-limited: slow down, you are asking too often"),
    ))
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert_eq!(record.served_event_ids().len(), 2, "two events, then CLOSED");
    assert_eq!(
        record.closed_sent(),
        vec![(
            record.reqs()[0].sub_id.clone(),
            "rate-limited: slow down, you are asking too often".to_string()
        )],
        "the relay's exact sentence reached the wire"
    );
    assert_eq!(rows.len(), 2, "and the two events before it reached the app");
}

/// Serve events the client never asked for. The REQ names one author; the
/// relay answers with another author's note as well.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_answers_with_an_event_that_does_not_match_the_filter() {
    let asked_about = Keys::generate();
    let stranger = Keys::generate();
    let intruder = note(&stranger, "nobody asked for this", 1_700_000_100);

    let relay = RelayLab::start(
        Script::new().seed(notes(&asked_about, 2, 1_700_000_000)).on_req(
            Req::kind(1),
            Reply::new()
                .then_stored()
                .then_events(vec![intruder.clone()])
                .then_eose(),
        ),
    )
    .await;

    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&asked_about, &relay), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(3));

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let record = relay.record();
    assert!(
        record.served_event_ids().contains(&intruder.id.to_hex()),
        "the relay really did put the unasked-for event on the wire"
    );
    assert!(
        !rows.iter().any(|row| row.id() == intruder.id),
        "an event outside the query's own filter must never appear as one of \
         its rows; rows: {:?}",
        rows.iter().map(nmp::Row::id).collect::<Vec<_>>()
    );
    // The positive control, in the same test rather than in a mutation
    // someone has to remember to run: an absence assertion is worthless
    // beside a pipeline that delivered nothing at all.
    assert_eq!(
        rows.len(),
        2,
        "the two events that DO match must still have arrived"
    );
}

/// Two dishonest bodies and one honest one, on the same subscription. The
/// relay serves a body that does not hash to the id it claims, a body whose
/// signature does not verify, and one real event. Only the real one may
/// become a row.
///
/// The honest event is the test's own positive control and is not optional:
/// `assert!(rows.is_empty())` beside a pipeline that delivered nothing at all
/// passes for the wrong reason, and there is no mutation anyone has to
/// remember to run to find that out.
#[tokio::test(flavor = "multi_thread")]
async fn a_forged_body_and_a_bad_signature_are_refused_while_an_honest_one_is_not() {
    let author = Keys::generate();
    let honest = note(&author, "what the author actually wrote", 1_700_000_000);
    let unsigned = note(&author, "never signed by this key", 1_700_000_001);
    let real = note(&author, "an ordinary, honest note", 1_700_000_002);

    let relay = RelayLab::start(Script::new().on_req(
        Req::kind(1),
        Reply::new()
            .then_events_json(vec![
                forge::different_body_same_id(&honest, "not what the author wrote"),
                forge::bad_signature(&unsigned),
            ])
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
    assert_eq!(
        record.served_event_ids().len(),
        3,
        "all three bodies really were put on the wire"
    );
    assert_eq!(
        rows.iter().map(nmp::Row::id).collect::<Vec<_>>(),
        vec![real.id],
        "exactly the honest event becomes a row, and exactly the two \
         dishonest ones do not"
    );
}

/// A NIP-42 challenge arriving in the MIDDLE of a live subscription, not at
/// connect. The relay serves two events, then challenges.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_challenges_partway_through_a_live_subscription() {
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
    assert_eq!(
        record.auth_challenges(),
        vec!["challenge-issued-mid-subscription".to_string()],
        "the challenge was issued after the stored phase, not at connect"
    );
    assert_eq!(
        rows.len(),
        2,
        "and the rows served before it are still the app's"
    );
}

/// Rate-limiting mid-stream: the relay serves part of what it holds, says so
/// in a NOTICE, and stops -- without CLOSED, without EOSE.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_rate_limits_in_the_middle_of_a_stream() {
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
    assert_eq!(record.served_event_ids().len(), 2);
    assert!(record.eosed_subscription_ids().is_empty());
    assert_eq!(rows.len(), 2);
}

/// Delay, per phase. The relay holds the answer for 700ms and then serves it.
/// The app must still receive it, and the wire must show the gap.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_may_take_as_long_as_the_script_says_before_answering() {
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

    // Nothing yet: the relay is still holding it.
    assert!(
        rows_within(&subscription, Duration::from_millis(300)).is_empty(),
        "the relay had not answered yet"
    );
    let rows = rows_within(&subscription, Duration::from_secs(3));
    assert_eq!(rows.len(), 3, "and then it did");

    relay.wire().wait_quiet(QUIET, SETTLE).await;
    assert_eq!(relay.record().eosed_subscription_ids().len(), 1);
}

/// A NIP-42 challenge at CONNECT, before the client has said anything -- the
/// common case, and the sibling of the mid-subscription challenge above.
///
/// NMP does NOT answer it. An unsolicited challenge on a session bound to
/// nobody is recorded and ignored: reads here authenticate as nobody, so
/// there is no identity to answer with and nothing yet that needs one. The
/// read is served regardless, because this relay gates nothing behind the
/// challenge it issued.
///
/// Asserting the silence rather than an AUTH response is deliberate. The
/// first draft of this test was named for the answer it expected and passed
/// without ever checking for one.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsolicited_connect_time_challenge_is_recorded_and_not_answered() {
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
    assert_eq!(
        record.auth_challenges(),
        vec!["challenge-at-connect".to_string()],
        "the challenge was issued before the client said anything"
    );
    assert!(
        record.auth_responses().is_empty(),
        "and NMP answered it with nothing: a challenge nothing depends on \
         needs no identity; got {:?}",
        record.auth_responses()
    );
    assert_eq!(
        rows.len(),
        2,
        "the unauthenticated read is served, because this relay gates nothing"
    );
}
