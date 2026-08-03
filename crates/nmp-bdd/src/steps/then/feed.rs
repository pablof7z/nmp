//! Assertions about the app-visible feed: which rows it shows, which rows it
//! must never show, and what an EMPTY feed is allowed to claim about itself.
//!
//! This is the only `Then` family that reads the delta channel an app itself
//! would subscribe to, so it is also the only one whose failures a product
//! person could see directly.

use crate::world::acquisition::{branch_shortfall, branch_sources};
use cucumber::then;

use crate::world::NmpWorld;

#[then(regex = r#"^my feed shows (\S+)'s notes$"#)]
async fn feed_shows_persons_notes(w: &mut NmpWorld, person: String) {
    let pk = w.pubkey_hex(&person);
    let shown = w.feed_eventually(|rows, _| rows.iter().any(|e| e.pubkey.to_hex() == pk));
    assert!(
        shown,
        "expected my feed to eventually show {person}'s notes"
    );
}

#[then(regex = r#"^my feed shows the note saying "([^"]+)"$"#)]
async fn feed_shows_note_text(w: &mut NmpWorld, text: String) {
    let shown = w.feed_eventually(|rows, _| rows.iter().any(|e| e.content == text));
    assert!(
        shown,
        "expected my feed to eventually show a note saying {text:?}"
    );
}

#[then(regex = r#"^notes from (\S+) no longer arrive$"#)]
async fn notes_no_longer_arrive(w: &mut NmpWorld, person: String) {
    nothing_to_observe!(
        w.feed_eventually(|rows, _| !rows.is_empty()),
        "my feed never held a single row, so nobody's notes could stop arriving from it"
    );
    let pk = w.pubkey_hex(&person);
    let pk_for_gone = pk.clone();
    let gone = w.feed_eventually(|rows, _| !rows.iter().any(|e| e.pubkey.to_hex() == pk_for_gone));
    assert!(
        gone,
        "expected {person}'s notes to eventually disappear from my feed"
    );
    let stays_gone = w.feed_never(|rows| rows.iter().any(|e| e.pubkey.to_hex() == pk));
    assert!(
        stays_gone,
        "expected {person}'s notes to never reappear in my feed"
    );
}

/// A COUNT, which is a claim no "shows the note saying ..." step can make:
/// two versions of one addressable coordinate both being present is only
/// observable as a number, because either one alone is also "shown".
#[then(regex = r#"^my feed holds exactly (\d+) rows$"#)]
async fn feed_holds_exactly_n_rows(w: &mut NmpWorld, n: usize) {
    let settled = w.feed_row_count_eventually(n);
    assert_eq!(
        settled,
        n,
        "expected my feed to hold exactly {n} rows; it holds {:?}",
        w.row_provenance()
    );
}

#[then(regex = r#"^my feed is empty$"#)]
async fn feed_is_empty(w: &mut NmpWorld) {
    let stays_empty = w.feed_never(|rows| !rows.is_empty());
    assert!(stays_empty, "expected my feed to stay empty");
}

#[then(regex = r#"^the query does not claim its empty result is complete$"#)]
async fn empty_result_is_not_claimed_complete(w: &mut NmpWorld) {
    // #49: there is no `Unknown` verdict and no authoritative-empty claim to
    // read. An empty feed is honest only while a planned source is still
    // unproven -- at least one source carries no `reconciled_through`
    // watermark (or the subtree surfaces a shortfall), so nothing presents
    // the emptiness as complete. The absence of any aggregate/`isComplete`
    // field is itself structural (there is no such surface to assert on).
    let not_claimed_complete = w.feed_eventually(|rows, evidence| {
        rows.is_empty()
            && (branch_sources(evidence).any(|s| s.reconciled_through.is_none())
                || branch_shortfall(evidence).next().is_some())
    });
    assert!(
        not_claimed_complete,
        "expected the empty feed to carry an unproven planned source \
         (no authoritative-empty / global-complete claim)"
    );
}
