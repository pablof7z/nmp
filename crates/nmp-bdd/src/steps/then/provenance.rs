//! Assertions about WHO DELIVERED a row: the per-relay source set the app
//! sees alongside every row on its feed.
//!
//! Its own family for the reason this module's parent doc gives -- the split
//! is by the DOMAIN of the claim, not the channel it reads. These read the
//! feed channel, like `feed` does, but they make a different claim about a
//! different subject: `feed` asks WHICH ROWS are shown, this asks WHERE EACH
//! ONE CAME FROM. The two answer independently -- a feed can show exactly the
//! right rows while attributing every one of them to the wrong relay, or to
//! nothing at all -- so an assertion about one is no evidence about the other.
//!
//! Every step here names its relays explicitly and compares the source set
//! EXACTLY. "Both relays served this" and "only this relay served it" are the
//! two facts these scenarios exist to separate, and a subset/superset test
//! would collapse them back together.

use cucumber::then;

use crate::steps::parse_quoted_list;
use crate::world::NmpWorld;

#[then(regex = r#"^the row saying "([^"]+)" names relays (.+) as its sources$"#)]
async fn row_names_relays_as_sources(w: &mut NmpWorld, content: String, list: String) {
    let names = parse_quoted_list(&list);
    assert!(
        names.len() > 1,
        "expected more than one quoted relay name in {list:?}; use the \
         single-source form for one"
    );
    assert_sources(w, &content, &names).await;
}

#[then(regex = r#"^the row saying "([^"]+)" names relay "([^"]+)" as its only source$"#)]
async fn row_names_relay_as_only_source(w: &mut NmpWorld, content: String, relay: String) {
    assert_sources(w, &content, &[relay]).await;
}

/// The one comparison both steps make, and the empty-world guard in front of
/// it: a provenance claim about a row that never arrived would fail for a
/// reason that has nothing to do with provenance, and (for the negative
/// spellings a future scenario may add) could pass for one too.
async fn assert_sources(w: &mut NmpWorld, content: &str, relay_names: &[String]) {
    nothing_to_observe!(
        w.feed_holds_row_saying(content),
        "no row saying {content:?} ever reached my feed, so there is no \
         provenance on it to check"
    );
    let expected = w.relay_urls(relay_names);
    assert!(
        w.row_sources_eventually(content, &expected),
        "expected the row saying {content:?} to name exactly {relay_names:?} \
         ({expected:?}) as its sources; the feed holds {:?}",
        w.row_provenance()
    );
}
