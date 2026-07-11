//! Falsifier + regression test for `docs/known-gaps.md`'s kind:10002
//! over-fetch finding (7,112 events received against a 39-author resolved
//! set). Discriminates the ACTUAL root cause with real diagnostic evidence
//! rather than assuming: is the wire filter's `authors` field ever
//! unscoped/missing (a wildcard-filter bug), or does `EngineCore::
//! sync_discovery` reopen the internal kind:10002 discovery subscription as
//! a fresh overwriting REQ every time an author's relay list resolves (a
//! churn bug -- each reopen is indistinguishable from a brand-new
//! subscription to a NIP-01-compliant relay, which replies with a fresh
//! EOSE burst re-sending every still-matching stored event)?
//!
//! Zero I/O: every "relay" interaction is a scripted `EngineMsg::RelayFrame`
//! fed directly to `EngineCore::handle`, exactly like
//! `tests/self_bootstrap_outbox.rs`.

use std::collections::BTreeSet;

use nmp_engine::core::{Effect, EngineCore, EngineMsg, RowDelta, RowSink};
use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector, TagName};
use nmp_resolver::LiveQuery;
use nmp_router::{LiveDirectory, WireOp};
use nmp_store::MemoryStore;
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, RelayMessage, RelayUrl, Tag, Tags, Timestamp};

struct NullSink;
impl RowSink for NullSink {
    fn on_rows(&self, _rows: Vec<RowDelta>) {}
}

fn connect(core: &mut EngineCore<MemoryStore>, slot: u32, url: &RelayUrl) -> Vec<Effect> {
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        url.clone(),
    ))
}

fn event_frame(sub: &str, event: nostr::Event) -> RelayFrame {
    RelayFrame::Text(RelayMessage::event(nostr::SubscriptionId::new(sub), event).as_json())
}

fn kind3(author: &Keys, follows: &[nostr::PublicKey], created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::ContactList, "")
        .tags(follows.iter().map(|pk| Tag::public_key(*pk)))
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

fn kind10002(author: &Keys, write: &RelayUrl, created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::RelayList, "")
        .tags(Tags::from_list(vec![Tag::relay_metadata(
            write.clone(),
            None,
        )]))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

fn follow_feed_query() -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::Tag(TagName::new('p').unwrap()),
        }))),
        ..Filter::default()
    })
}

/// Every `WireOp::Req` this test has observed for the kind:10002 discovery
/// skeleton, per (relay) -- collected across the WHOLE run so the test can
/// sum "how many authors would a NIP-01-compliant relay have re-sent, in
/// total, across every overwriting REQ this engine ever issued for its
/// internal discovery sub" -- the exact mechanism that turns N authors into
/// far more than N received events if the sub is torn down and reopened on
/// every single resolution.
#[derive(Default)]
struct DiscoveryReqLog {
    /// One entry per `WireOp::Req` seen for kind:10002, in emission order:
    /// the author-set SIZE that REQ carried.
    per_req_author_counts: Vec<usize>,
    /// The exact wire JSON of the LAST kind:10002 filter observed (steady
    /// state) -- to directly answer "was the authors field ever missing or
    /// broadened to a wildcard?"
    last_filter_json: Option<String>,
}

impl DiscoveryReqLog {
    fn observe(&mut self, effects: &[Effect]) {
        for effect in effects {
            if let Effect::Wire(delta) = effect {
                for (_relay, ops) in &delta.ops {
                    for op in ops {
                        if let WireOp::Req(_sub_id, filter) = op {
                            if filter.kinds == Some(BTreeSet::from([10_002u16])) {
                                let n = filter.authors.as_ref().map(|a| a.len()).unwrap_or(0);
                                self.per_req_author_counts.push(n);
                                self.last_filter_json = Some(filter.to_nostr().as_json());
                            }
                        }
                    }
                }
            }
        }
    }

    fn total_req_count(&self) -> usize {
        self.per_req_author_counts.len()
    }

    /// Sum of every REQ's author-set size -- the total author-events a
    /// NIP-01-compliant relay would resend across the whole run, since an
    /// overwriting REQ on an already-open sub-id is indistinguishable from a
    /// brand-new subscription (full EOSE replay).
    fn total_author_resends(&self) -> usize {
        self.per_req_author_counts.iter().sum()
    }
}

/// THE discriminating falsifier: 39 authors (matching the real on-device
/// finding's scale) each resolve their kind:10002 ONE AT A TIME (exactly how
/// a live relay delivers them -- one `EVENT` frame per stored event, never
/// batched) against a single indexer. Two things are checked independently:
///
/// 1. Is the wire filter's `authors` field EVER missing/empty/broadened to
///    a wildcard while authors remain needed? (the original "unscoped
///    filter" hypothesis -- this test proves it FALSE: every observed
///    kind:10002 Req carries a properly-scoped, non-empty `authors` set).
/// 2. Is the discovery sub torn down and reopened as a fresh Req on every
///    single resolution, so a NIP-01-compliant relay would have to resend
///    its stored kind:10002 for every author still in the (shrinking) set,
///    each time? Summed over 39 sequential resolutions this is a triangular
///    number (39+38+...+1 = 780) -- NOT 39 -- even though only 39 authors
///    were ever resolved. This is the actual, falsifiable churn mechanism
///    behind `docs/known-gaps.md`'s 7,112-events-for-39-authors finding.
#[test]
fn resolving_39_authors_one_at_a_time_does_not_churn_the_discovery_sub() {
    const N: usize = 39;
    let me = Keys::generate();
    let authors: Vec<Keys> = (0..N).map(|_| Keys::generate()).collect();
    let indexer = RelayUrl::parse("wss://indexer.example.com").unwrap();

    let dir = LiveDirectory::new([indexer.clone()]);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);
    let mut log = DiscoveryReqLog::default();

    log.observe(&connect(&mut core, 0, &indexer));
    log.observe(&core.handle(EngineMsg::SetActivePubkey(Some(me.public_key()))));
    log.observe(&core.handle(EngineMsg::Subscribe(
        follow_feed_query(),
        Box::new(NullSink),
    )));

    // `me` follows all 39 synthetic authors in one shot (one kind:3, exactly
    // like a real contact list).
    let follows: Vec<nostr::PublicKey> = authors.iter().map(Keys::public_key).collect();
    log.observe(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", kind3(&me, &follows, 100)),
    )));

    // Each of the 39 authors' kind:10002 arrives SEPARATELY (its own
    // `RelayMessage::Event` frame), staggered over time -- exactly how a
    // real relay streams stored events back, one at a time, never as a
    // single batch.
    let write_relay = RelayUrl::parse("wss://writes.example.com").unwrap();
    log.observe(&connect(&mut core, 1, &write_relay));
    for (i, author) in authors.iter().enumerate() {
        let relay_list = kind10002(author, &write_relay, 200 + i as u64);
        log.observe(&core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            event_frame("s", relay_list),
        )));
    }

    // -- 1. wildcard-filter hypothesis: FALSE. Every kind:10002 Req this
    // engine ever emitted carried a non-empty, properly-scoped `authors`
    // set -- never missing, never empty while authors remained needed.
    assert!(
        !log.per_req_author_counts.is_empty(),
        "the engine must have opened at least one kind:10002 discovery Req"
    );
    assert!(
        log.per_req_author_counts.iter().all(|&n| n > 0),
        "every kind:10002 Req must carry a non-empty `authors` set -- an \
         unscoped/wildcard discovery filter never reached the wire in this run: {:?}",
        log.per_req_author_counts
    );
    if let Some(json) = &log.last_filter_json {
        assert!(
            json.contains("\"authors\""),
            "the exact wire JSON of the last kind:10002 filter must carry an \
             `authors` field: {json}"
        );
    }

    // -- 2. churn hypothesis: the load-bearing regression assertion. If the
    // discovery sub were torn down and reopened on every single resolution
    // (dropping the just-resolved author from the filter each time), a
    // NIP-01-compliant relay would have resent the triangular sum
    // 39+38+...+1 = 780 total author-events across the whole run -- almost
    // 20x the 39 authors actually being discovered. The fix keeps this at
    // O(N): a small, BOUNDED number of Reqs (never one per resolution), so
    // the total resend volume stays within a small constant factor of N.
    let triangular_39: usize = (1..=N).sum();
    println!(
        "kind:10002 discovery: {} total authors, {} total Req ops, {} total \
         author-resends across the whole run (pre-fix churn ceiling would be {})",
        N,
        log.total_req_count(),
        log.total_author_resends(),
        triangular_39
    );
    assert!(
        log.total_author_resends() <= N * 3,
        "resolving {N} authors one at a time caused {} total author-resends \
         across {} Req ops -- the discovery sub is being torn down and \
         reopened on every single resolution (each reopen is a fresh, \
         NIP-01-indistinguishable-from-new subscription that a relay replies \
         to with a full resend). Expected O(N) (<= {}), not O(N^2) \
         (triangular ceiling would be {triangular_39}).",
        log.total_author_resends(),
        log.total_req_count(),
        N * 3,
    );
}
