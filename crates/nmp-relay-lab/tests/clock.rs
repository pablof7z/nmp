//! What a scenario can say about *now*. Read `nmp_relay_lab::clock` first:
//! this file is the executable half of the finding recorded there, and the
//! finding is currently that there is nothing to execute.

mod support;

use nmp_relay_lab::{RelayLab, Reply, Script};
use nmp_relay_lab::Ev;
use nostr::{Keys, Timestamp};
use support::{publish_note, publishing_engine, QUIET, SETTLE};

/// Every write an app asks NMP to time carries the REAL clock, and no
/// scenario can change that through the product facade.
///
/// This test used to have three siblings that stated an instant, moved the
/// clock backwards across two writes, and advanced it thirty days. All three
/// went through `Engine::clock()`, which no longer exists: the
/// `unstable-mechanism` feature that gated it was deleted along with the
/// testkit crates, and `crates/nmp/src/` now contains no clock door of any
/// kind. What is left is this -- the observation that the stamp is the real
/// clock -- and it is now unfalsifiable from the app's side, which is the
/// point.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_carries_the_real_clock_and_the_facade_offers_no_way_to_say_otherwise() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().on_event(Ev::any(), Reply::ok())).await;
    let engine = publishing_engine(&relay, &author);

    let before = Timestamp::now();
    let _receipt = publish_note(&engine, "published on whatever clock the machine has");
    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await
    );
    let after = Timestamp::now();

    let held = relay.held();
    assert_eq!(held.len(), 1);
    assert!(
        held[0].created_at >= before && held[0].created_at <= after,
        "the stamp is the real clock: {:?} is not within {before:?}..={after:?}",
        held[0].created_at
    );

    relay.wire().wait_quiet(QUIET, SETTLE).await;
}
