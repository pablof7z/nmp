//! #1715 capstone: bookmarks compose through the ordinary `WriteIntent` /
//! `publish` / receipt / `LiveQuery` path AND fold back into a typed
//! [`BookmarksList`] over the ordinary observation surface -- against a
//! real engine, proving the whole capability end to end rather than each
//! half in isolation.

use std::time::{Duration, Instant};

use nmp::{Engine, EngineConfig, LiveQuery, RowDelta};
use nmp_bookmarks::{
    add_bookmark, bookmark_capability, bookmark_writes, current_account_bookmarks_demand,
    parse_bookmarks_tolerant, remove_bookmark, BookmarkedItem,
};
use nostr::Keys;

/// Wait for the current account's kind:10003 row to reach a state
/// `matches` accepts, and hand back the typed list it parses to. Polling
/// rather than a single `recv` because a durable semantic write settles
/// over more than one delta (pending, then signed).
fn wait_for_bookmarks(
    subscription: &nmp::Subscription,
    matches: impl Fn(&nmp_bookmarks::BookmarksList) -> bool,
) -> nmp_bookmarks::BookmarksList {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the expected bookmarks list never appeared"
        );
        let frame = subscription
            .recv_timeout(remaining)
            .expect("query stays open through the whole scenario");
        for delta in frame.deltas {
            let row = match delta {
                RowDelta::Added(row) | RowDelta::Updated(row) => row,
                _ => continue,
            };
            let list = parse_bookmarks_tolerant(&row.event_for_store());
            if matches(&list) {
                return list;
            }
        }
    }
}

#[test]
fn add_and_remove_bookmarks_compose_and_fold_back_through_a_real_engine() {
    let engine =
        Engine::new_with_capabilities(EngineConfig::default(), vec![bookmark_capability()])
            .expect("a temporary Redb engine builds");
    let author = Keys::generate();
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("author is current without installing a signer");
    let writes = bookmark_writes();

    let subscription = engine
        .observe(LiveQuery::single(current_account_bookmarks_demand()), None)
        .expect("the reactive bookmarks demand is reachable and usable from nmp-bookmarks alone");

    let article = BookmarkedItem::Url("https://example.com/great-post".to_string());
    let receipt = add_bookmark(&engine, &writes, article.clone())
        .expect("the first bookmark enters ordinary custody");
    let after_add = wait_for_bookmarks(&subscription, |list| list.items.contains(&article));
    assert_eq!(after_add.items, vec![article.clone()]);
    assert!(
        engine
            .publish_queue(None, 10)
            .unwrap()
            .iter()
            .any(|entry| entry.receipt_id == receipt.id),
        "the add must name a real queue entry"
    );

    let topic = BookmarkedItem::Hashtag("nostr".to_string());
    add_bookmark(&engine, &writes, topic.clone()).expect("the second bookmark is accepted");
    let after_second_add = wait_for_bookmarks(&subscription, |list| {
        list.items.len() == 2 && list.items.contains(&topic)
    });
    assert_eq!(after_second_add.items, vec![article.clone(), topic.clone()]);

    remove_bookmark(&engine, &writes, article.clone()).expect("the removal is accepted");
    let after_remove = wait_for_bookmarks(&subscription, |list| !list.items.contains(&article));
    assert_eq!(after_remove.items, vec![topic]);

    engine.shutdown();
}
