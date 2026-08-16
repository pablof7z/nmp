//! The global stalled-write list: what it names, what it says about each
//! entry, and what it must never do.
//!
//! A different domain from [`super::routes`] and [`super::writes`], which
//! both read a RECEIPT -- and the difference is the whole reason this
//! surface exists. A receipt answers "what happened to THIS write", which is
//! only useful to someone still holding it; every assertion here is made by
//! an app holding nothing, reading one engine-global list.

use cucumber::then;

use nmp_engine::core::{StalledWrite, StalledWriteStage};
use nmp_router::RelayUrl;

use crate::world::NmpWorld;

/// The one row a scenario is talking about.
///
/// Every scenario in this family stalls exactly one write per stage, so
/// "that write" is unambiguous -- but reading it as "whatever happens to be
/// first" would make a list of the wrong length pass. This insists there is
/// exactly one row and hands it back.
fn only_row(w: &mut NmpWorld) -> StalledWrite {
    let rows = w.stalled_writes();
    nothing_to_observe!(
        !rows.is_empty(),
        "the stalled-write list is empty after waiting {:?}, so it names nothing either way",
        NmpWorld::stalled_read_budget()
    );
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one stalled write for this scenario to be talking about; saw {rows:?}"
    );
    rows.into_iter().next().expect("length checked above")
}

fn stage_word(stage: StalledWriteStage) -> &'static str {
    match stage {
        StalledWriteStage::Unroutable => "unroutable",
        StalledWriteStage::Unsignable => "unsignable",
        StalledWriteStage::Undeliverable => "undeliverable",
    }
}

// ---- what the list names -------------------------------------------------

#[then(regex = r#"^stalled writes names that write$"#)]
async fn names_that_write(w: &mut NmpWorld) {
    let row = only_row(w);
    assert!(
        !row.detail.is_empty(),
        "a stalled entry with an empty reason is barely better than an absent one: an app can \
         render it and a person can read it, and neither learns anything"
    );
    w.remember_named_stalled_write(&row.id);
}

#[then(regex = r#"^stalled writes does not name that write$"#)]
async fn does_not_name_that_write(w: &mut NmpWorld) {
    let named = w
        .named_stalled_write()
        .expect("nmp-bdd: nothing named a stalled write for this step to miss")
        .to_string();
    w.read_stalled_writes_until_empty();
    let still_there = w.stalled_writes().iter().any(|row| row.id == named);
    assert!(
        !still_there,
        "the list has to be able to empty, or an app learns to ignore it; {named} is still \
         listed as {:?}",
        w.stalled_writes()
    );
}

#[then(regex = r#"^stalled writes reports (\d+) writes?$"#)]
async fn reports_n_writes(w: &mut NmpWorld, expected: u64) {
    let totals = w.stalled_write_totals();
    let counted = totals
        .unroutable
        .saturating_add(totals.unsignable)
        .saturating_add(totals.undeliverable);
    nothing_to_observe!(
        counted > 0 || expected == 0,
        "the stalled-write census is empty after waiting {:?}",
        NmpWorld::stalled_read_budget()
    );
    assert_eq!(
        counted,
        expected,
        "expected the census to count {expected} stalled writes; saw {totals:?} with rows {:?}",
        w.stalled_writes()
    );
    assert_eq!(
        totals.omitted_details, 0,
        "a scenario this small must fit the detail window whole, or its per-entry assertions \
         would be reading a truncation rather than the list"
    );
}

#[then(regex = r#"^it reports the write as (unroutable|unsignable|undeliverable)$"#)]
async fn reports_stage(w: &mut NmpWorld, expected: String) {
    let row = only_row(w);
    assert_eq!(
        stage_word(row.stage),
        expected,
        "expected the write to be reported as {expected}; saw {row:?}"
    );
}

#[then(regex = r#"^one of them is reported as (unroutable|unsignable|undeliverable)$"#)]
async fn one_of_them_reports_stage(w: &mut NmpWorld, expected: String) {
    let rows = w.stalled_writes();
    nothing_to_observe!(
        !rows.is_empty(),
        "the stalled-write list is empty, so no entry is at any stage"
    );
    let matching = rows
        .iter()
        .filter(|row| stage_word(row.stage) == expected)
        .count();
    assert_eq!(
        matching, 1,
        "expected exactly one entry reported as {expected}; saw {rows:?}"
    );
}

#[then(regex = r#"^each of them reports its own reason$"#)]
async fn each_reports_its_own_reason(w: &mut NmpWorld) {
    let rows = w.stalled_writes();
    nothing_to_observe!(
        !rows.is_empty(),
        "the stalled-write list is empty, so no entry has a reason either way"
    );
    let mut reasons: Vec<&str> = rows.iter().map(|row| row.detail.as_str()).collect();
    assert!(
        reasons.iter().all(|reason| !reason.is_empty()),
        "every entry has to say what it is waiting for; saw {rows:?}"
    );
    reasons.sort_unstable();
    let distinct = reasons.len();
    reasons.dedup();
    assert_eq!(
        reasons.len(),
        distinct,
        "three writes stuck for three unrelated reasons must not share one sentence; saw {rows:?}"
    );
}

#[then(regex = r#"^it reports the reason as "([^"]+)" being unreachable$"#)]
async fn reason_names_unreachable_relay(w: &mut NmpWorld, url: String) {
    let url = RelayUrl::parse(&url).expect("nmp-bdd: a scenario names a real relay URL");
    let row = only_row(w);
    assert!(
        row.detail.contains(url.as_str()),
        "expected the reason to name {url} as the destination nothing answers for; saw {:?}",
        row.detail
    );
}

// ---- the age, and what NMP declines to do with it ------------------------

#[then(regex = r#"^it reports how long the write has been stalled$"#)]
async fn reports_how_long(w: &mut NmpWorld) {
    let row = only_row(w);
    let now = w.reader_now();
    assert!(
        row.stalled_since.as_secs() > 0 && row.stalled_since.as_secs() <= now,
        "expected an acceptance instant this reader can subtract a real age from; saw {:?} \
         against a reader clock of {now}",
        row.stalled_since
    );
}

/// The age is the READER's subtraction, against the same clock the
/// acceptance was stamped by -- the engine's stated one, which the matching
/// `<n> days pass` step advanced. NMP never computes this number, which is
/// the whole point of the scenario it serves.
#[then(regex = r#"^it reports the write as stalled for about (\d+) (seconds|days)$"#)]
async fn stalled_for_about(w: &mut NmpWorld, amount: u64, unit: String) {
    let expected = match unit.as_str() {
        "seconds" => amount,
        "days" => amount * 86_400,
        other => panic!("nmp-bdd: unsupported elapsed unit {other:?}"),
    };
    let row = only_row(w);
    let now = w.reader_now();
    let age = now.saturating_sub(row.stalled_since.as_secs());
    // "About", because the scenario also spends real time between accepting
    // the write and advancing the clock. It can only ever be LONGER than what
    // was stated, never shorter -- a shorter age means the acceptance instant
    // moved, which is the failure this asserts against.
    assert!(
        age >= expected,
        "expected the write to read as stalled for at least {expected}s; the list says it was \
         accepted at {:?} and this reader's clock reads {now}",
        row.stalled_since
    );
}

#[then(regex = r#"^NMP has drawn no conclusion from how long it has been stalled$"#)]
async fn no_conclusion_from_age(w: &mut NmpWorld) {
    // The only conclusion NMP could draw is to stop holding the obligation.
    // It is still listed, with the same reason, at the same acceptance
    // instant -- and its receipt never reported a failure.
    let row = only_row(w);
    assert!(
        !row.detail.is_empty(),
        "an aged entry keeps the reason it was parked with; saw {row:?}"
    );
    let failed = w.never_failed();
    assert!(
        failed,
        "interpreting the age is deciding to give up, and giving up is the app's decision or \
         the person's, never a timer's; receipt showed {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^nothing abandoned the write on NMP's own initiative$"#)]
async fn nothing_abandoned_it(w: &mut NmpWorld) {
    let terminal = w.never_failed()
        && w.receipt_never(|seen| {
            seen.iter().any(|status| {
                matches!(
                    status,
                    nmp_engine::publish_queue::WriteFact::Outcome(
                        nmp_engine::publish_queue::WriteOutcome::NotSent(
                            nmp_engine::publish_queue::NotSentReason::Cancelled
                        )
                    )
                )
            })
        });
    assert!(
        terminal,
        "explicit cancellation is the one abandonment door, and nobody opened it; receipt \
         showed {:?}",
        w.receipt_statuses()
    );
}

// ---- the list is evidence, and evidence does not act ---------------------

#[then(regex = r#"^no delivery attempt was made by reading diagnostics$"#)]
async fn reading_made_no_attempt(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.repeated_stalled_fingerprints()
            .first()
            .is_some_and(|rows| !rows.is_empty()),
        "the reads returned an empty list, so reading it is blameless only because there was \
         nothing to read"
    );
    let strays: Vec<String> = w
        .relay_names()
        .filter(|name| !w.relay_untouched_since_snapshot(name))
        .cloned()
        .collect();
    assert!(
        strays.is_empty(),
        "if reading retried, an app that polled would publish differently from one that did \
         not; {strays:?} were touched -- {}",
        strays
            .iter()
            .map(|name| format!("{name}: {}", w.touch_report_since_snapshot(name)))
            .collect::<Vec<_>>()
            .join("; ")
    );
}

#[then(regex = r#"^no write changed state$"#)]
async fn no_write_changed_state(w: &mut NmpWorld) {
    let fingerprints = w.repeated_stalled_fingerprints().to_vec();
    nothing_to_observe!(
        fingerprints.first().is_some_and(|rows| !rows.is_empty()),
        "the reads returned an empty list, so 'they all agreed' is a statement about nothing"
    );
    let first = &fingerprints[0];
    assert!(
        fingerprints.iter().all(|seen| seen == first),
        "every read of a mirror must show the same thing; the {} reads disagreed: {fingerprints:?}",
        fingerprints.len()
    );
}

#[then(regex = r#"^nothing durable was recorded$"#)]
async fn nothing_durable_was_recorded(w: &mut NmpWorld) {
    // The durable facts a read could have created would arrive on the
    // receipt as new statuses. Settle the full negative budget and compare
    // what the stream holds against what the repeated reads described.
    let before = w.stalled_writes();
    nothing_to_observe!(
        !before.is_empty(),
        "the list is empty, so an unchanged descriptor and instant are unchanged only because \
         there are none"
    );
    let after = w.receipt_statuses_after_settling();
    assert!(
        !after.iter().any(|status| matches!(
            status,
            nmp_engine::publish_queue::WriteFact::Relay {
                state: nmp_engine::publish_queue::RelayState::Published,
                ..
            }
        )),
        "a read cannot have delivered anything; receipt showed {after:?}"
    );
    w.read_stalled_writes();
    assert_eq!(
        w.stalled_writes()
            .iter()
            .map(|row| (row.id.clone(), row.stalled_since))
            .collect::<Vec<_>>(),
        before
            .iter()
            .map(|row| (row.id.clone(), row.stalled_since))
            .collect::<Vec<_>>(),
        "the descriptor and acceptance instant are derived from durable facts, so a read that \
         wrote anything would have moved them"
    );
}
