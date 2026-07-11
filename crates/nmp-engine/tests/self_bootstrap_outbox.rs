//! The self-bootstrapping outbox (M5, `docs/known-gaps.md`'s
//! "RelayDirectory" gap): headless, deterministic proof that `EngineCore`
//! discovers an author's NIP-65 write relays on its own from a live
//! `nmp_router::LiveDirectory` + a configured indexer set -- the app never
//! resolves any relay itself.
//!
//! Zero I/O: every "relay" interaction here is a scripted
//! `EngineMsg::RelayFrame` fed directly to `EngineCore::handle`, exactly
//! like `tests/core_headless.rs`. `EngineCore` exposes no `plan()` accessor,
//! so [`PlanModel`] reconstructs the authoritative current per-relay plan
//! purely by replaying every `Effect::Wire` delta this test has seen --
//! exactly what a real runtime's own wire-frame bookkeeping does.

use std::collections::{BTreeMap, BTreeSet};

use nmp_engine::core::{Effect, EngineCore, EngineMsg, RowDelta, RowSink};
use nmp_grammar::{Binding, ConcreteFilter, Derived, Filter, IdentityField, Selector, TagName};
use nmp_resolver::LiveQuery;
use nmp_router::{LiveDirectory, SubId, WireOp};
use nmp_store::MemoryStore;
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::nips::nip65::RelayMetadata;
use nostr::{
    EventBuilder, JsonUtil, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Tag, Tags,
    Timestamp,
};

/// A `RowSink` that discards everything -- these tests only care about
/// wire-routing effects, not row delivery.
struct NullSink;
impl RowSink for NullSink {
    fn on_rows(&self, _rows: Vec<RowDelta>) {}
}

/// Replays every `Effect::Wire` delta into the authoritative current
/// per-relay plan (mirrors `nmp_router::RelayPlan` -- `EngineCore` has no
/// public accessor for its own internal `Router::plan()`, so a test has to
/// reconstruct it the same way a real runtime's wire-frame bookkeeping
/// does: `Req` inserts/replaces a `(relay, sub_id)` entry, `Close` removes
/// it).
#[derive(Default)]
struct PlanModel(BTreeMap<(RelayUrl, SubId), ConcreteFilter>);

impl PlanModel {
    fn apply(&mut self, effects: &[Effect]) {
        for effect in effects {
            if let Effect::Wire(delta) = effect {
                for (relay, ops) in &delta.ops {
                    for op in ops {
                        match op {
                            WireOp::Req(sub_id, filter) => {
                                self.0
                                    .insert((relay.clone(), sub_id.clone()), filter.clone());
                            }
                            WireOp::Close(sub_id) => {
                                self.0.remove(&(relay.clone(), sub_id.clone()));
                            }
                        }
                    }
                }
            }
        }
    }

    fn reqs_for(&self, relay: &RelayUrl) -> Vec<&ConcreteFilter> {
        self.0
            .iter()
            .filter(|((r, _), _)| r == relay)
            .map(|(_, f)| f)
            .collect()
    }

    fn sub_id_for_kind(&self, relay: &RelayUrl, kind: u16) -> Option<SubId> {
        self.0
            .iter()
            .find(|((r, _), f)| r == relay && f.kinds == Some(BTreeSet::from([kind])))
            .map(|((_, sub_id), _)| sub_id.clone())
    }
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
    RelayFrame::Text(RelayMessage::event(SubscriptionId::new(sub), event).as_json())
}

/// A kind:3 (contact list) event: `follows` encoded as `p` tags.
fn kind3(author: &Keys, follows: &[nostr::PublicKey], created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::ContactList, "")
        .tags(follows.iter().map(|pk| Tag::public_key(*pk)))
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:10002 (NIP-65 relay list) event declaring `write` as the sole
/// (read+write) relay for `author`.
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

/// A kind:10002 event declaring `write` as an EXPLICIT write-only relay
/// alongside `read_only` as an explicit read-only relay (proves the write
/// parse excludes an explicit `"read"` marker).
fn kind10002_with_read_relay(
    author: &Keys,
    write: &RelayUrl,
    read_only: &RelayUrl,
    created_at: u64,
) -> nostr::Event {
    EventBuilder::new(Kind::RelayList, "")
        .tags(Tags::from_list(vec![
            Tag::relay_metadata(write.clone(), Some(RelayMetadata::Write)),
            Tag::relay_metadata(read_only.clone(), Some(RelayMetadata::Read)),
        ]))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// The `$myFollows` shape: kind:1 authored by whoever the active pubkey's
/// kind:3 contact list currently names (identical shape to `nmp-demo`'s
/// `build_follow_feed_query`).
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

/// THE headline falsifier: configure the engine with ONLY an indexer set
/// (no pre-resolved write relays at all), subscribe to the follow-feed,
/// ingest the account's kind:3 (naming follow A), then A's kind:10002
/// (declaring write relay R) -- and prove:
/// 1. before A's kind:10002 arrives, the engine has already opened an
///    internal kind:10002 discovery REQ for A against the indexer, AND A's
///    kind:1 content atom has NO route at all (zero wire subs -- never an
///    indexer fallback for content);
/// 2. after A's kind:10002 lands, A's kind:1 content atom routes to R, not
///    the indexer.
#[test]
fn content_atom_reroutes_from_indexer_discovery_to_authors_write_relay_after_10002_arrives() {
    let me = Keys::generate();
    let a = Keys::generate();
    let indexer = RelayUrl::parse("wss://indexer.example.com").unwrap();
    let write_r = RelayUrl::parse("wss://a-writes-here.example.com").unwrap();

    let dir = LiveDirectory::new([indexer.clone()]);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);
    let mut plan = PlanModel::default();

    plan.apply(&connect(&mut core, 0, &indexer));
    plan.apply(&core.handle(EngineMsg::SetActivePubkey(Some(me.public_key()))));
    plan.apply(&core.handle(EngineMsg::Subscribe(
        follow_feed_query(),
        Box::new(NullSink),
    )));

    // No follow named yet: `me`'s own kind:3 (a discovery-kind atom in its
    // own right, per `nmp_router::DiscoveryKinds`) and the engine's own
    // kind:10002 discovery for `me` are legitimately already on the wire at
    // the indexer -- but A does not exist in ANY demand yet, so there must
    // be no discovery REQ naming A, and no kind:1 content atom anywhere.
    assert!(
        !plan
            .reqs_for(&indexer)
            .iter()
            .any(|f| f.authors == Some(BTreeSet::from([a.public_key().to_hex()]))),
        "A isn't demanded by anything yet -- nothing about A should be on the wire"
    );
    assert!(
        !plan
            .0
            .values()
            .any(|f| f.kinds == Some(BTreeSet::from([1u16]))),
        "no kind:1 content atom exists before A is ever followed"
    );

    // `me`'s kind:3 names A as a follow -- A's kind:1 atom now exists, but
    // A's write relays are unknown, so (a) the content atom must route
    // NOWHERE (never an indexer fallback) and (b) the engine must have
    // opened its OWN internal kind:10002 discovery REQ against the indexer
    // for A.
    let contacts = kind3(&me, &[a.public_key()], 100);
    plan.apply(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", contacts),
    )));

    // The discovery atom's author set also includes `me` (the engine
    // discovers its OWN write relays too, since `me`'s kind:3 atom is
    // itself outbox-classified and `me`'s write relays are equally unknown
    // -- a nice generalization of the same mechanism, not a special case
    // for followed authors only), so this asserts A is a MEMBER, not that
    // the set is exactly `{A}`.
    let indexer_reqs = plan.reqs_for(&indexer);
    assert!(
        indexer_reqs
            .iter()
            .any(|f| f.kinds == Some(BTreeSet::from([10_002u16]))
                && f.authors
                    .as_ref()
                    .is_some_and(|a_set| a_set.contains(&a.public_key().to_hex()))),
        "engine must self-open a kind:10002 discovery REQ against the indexer covering A; \
         got: {indexer_reqs:?}"
    );
    assert!(
        !indexer_reqs
            .iter()
            .any(|f| f.kinds == Some(BTreeSet::from([1u16]))),
        "A's kind:1 content atom must NOT be routed to the indexer -- indexers are \
         never a content fallback"
    );
    assert!(
        !plan
            .0
            .values()
            .any(|f| f.kinds == Some(BTreeSet::from([1u16]))),
        "a content atom for an author with no known write relays has zero wire subs \
         anywhere at all"
    );

    // A's kind:10002 arrives (over the SAME indexer connection, exactly as
    // a real discovery REQ's reply would): parse -> live-directory update ->
    // recompile -> A's kind:1 atom must now route to R.
    plan.apply(&connect(&mut core, 1, &write_r));
    let relay_list = kind10002(&a, &write_r, 200);
    plan.apply(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", relay_list),
    )));

    // `f.kinds` CONTAINS 1 rather than equals exactly `{1}`: the discovery
    // sub is widen-only (`sync_discovery`'s fix for the kind:10002
    // over-fetch churn, `docs/known-gaps.md`) so A is deliberately left in
    // its author set even after resolving -- and write_r is now ALSO a
    // legitimate discovery-lane candidate for A (additive relay roles, Bug
    // 3: an author's own write relay is a fine place to also ask for their
    // kind:10002). `nmp_router::coalesce`'s `KindUnion` rule correctly folds
    // both onto ONE Req rather than opening two redundant subs to the same
    // relay -- this is the widen-safe merge working exactly as designed,
    // not a partial route.
    assert!(
        plan.reqs_for(&write_r).iter().any(|f| f
            .kinds
            .as_ref()
            .is_some_and(|ks| ks.contains(&1u16))
            && f.authors == Some(BTreeSet::from([a.public_key().to_hex()]))),
        "A's kind:1 content atom must now be routed to A's OWN write relay R; got: {:?}",
        plan.reqs_for(&write_r)
    );
    assert!(
        !plan
            .reqs_for(&indexer)
            .iter()
            .any(|f| f.kinds.as_ref().is_some_and(|ks| ks.contains(&1u16))),
        "the indexer must never carry a content-kind REQ, even after discovery completes"
    );
}

/// The write-relay parse excludes an explicit `"read"`-marked relay, same
/// NIP-65 semantics `nmp-demo`'s former bootstrap phase used.
#[test]
fn relay_list_parse_excludes_explicit_read_only_relays() {
    let me = Keys::generate();
    let a = Keys::generate();
    let indexer = RelayUrl::parse("wss://indexer.example.com").unwrap();
    let write_r = RelayUrl::parse("wss://a-writes-here.example.com").unwrap();
    let read_only_r = RelayUrl::parse("wss://a-reads-here.example.com").unwrap();

    let dir = LiveDirectory::new([indexer.clone()]);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);
    let mut plan = PlanModel::default();

    plan.apply(&connect(&mut core, 0, &indexer));
    plan.apply(&connect(&mut core, 1, &write_r));
    plan.apply(&connect(&mut core, 2, &read_only_r));
    plan.apply(&core.handle(EngineMsg::SetActivePubkey(Some(me.public_key()))));
    plan.apply(&core.handle(EngineMsg::Subscribe(
        follow_feed_query(),
        Box::new(NullSink),
    )));
    plan.apply(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", kind3(&me, &[a.public_key()], 100)),
    )));

    let relay_list = kind10002_with_read_relay(&a, &write_r, &read_only_r, 200);
    plan.apply(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", relay_list),
    )));

    // `contains(&1)` rather than `== Some({1})`: the widen-only discovery
    // sub (see the churn fix's doc on `sync_discovery`) leaves A in its
    // author set after resolving, and write_r is now ALSO a legitimate
    // discovery-lane candidate for A (additive relay roles) -- `KindUnion`
    // may correctly fold that onto the same Req as A's kind:1 content atom.
    assert!(
        plan.reqs_for(&write_r)
            .iter()
            .any(|f| f.kinds.as_ref().is_some_and(|ks| ks.contains(&1u16))),
        "the write relay must carry A's content atom; got: {:?}",
        plan.reqs_for(&write_r)
    );
    assert!(
        !plan
            .reqs_for(&read_only_r)
            .iter()
            .any(|f| f.kinds.as_ref().is_some_and(|ks| ks.contains(&1u16))),
        "an explicit read-only relay must never receive A's content atom"
    );
}

/// Reactivity: when the demanded author set grows (a second follow is
/// added), the engine's internal discovery subscription grows to cover the
/// new author too -- via the SAME sub-id (kind:10002's skeleton never
/// changes), an overwriting REQ, not a parallel close+reopen.
#[test]
fn discovery_grows_reactively_as_the_follow_set_grows() {
    let me = Keys::generate();
    let a = Keys::generate();
    let b = Keys::generate();
    let indexer = RelayUrl::parse("wss://indexer.example.com").unwrap();

    let dir = LiveDirectory::new([indexer.clone()]);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);
    let mut plan = PlanModel::default();

    plan.apply(&connect(&mut core, 0, &indexer));
    plan.apply(&core.handle(EngineMsg::SetActivePubkey(Some(me.public_key()))));
    plan.apply(&core.handle(EngineMsg::Subscribe(
        follow_feed_query(),
        Box::new(NullSink),
    )));

    plan.apply(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", kind3(&me, &[a.public_key()], 100)),
    )));
    let sub_id_first = plan
        .sub_id_for_kind(&indexer, 10_002)
        .expect("discovery REQ for A must exist");

    // `me` now follows B too -- the discovery atom must widen to {A, B}
    // under the SAME sub-id (kind:10002's skeleton is authors-erased), a
    // single overwriting REQ.
    plan.apply(&core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", kind3(&me, &[a.public_key(), b.public_key()], 150)),
    )));

    let widened = plan
        .reqs_for(&indexer)
        .into_iter()
        .find(|f| f.kinds == Some(BTreeSet::from([10_002u16])))
        .expect("a widened discovery REQ must still exist");
    // `me`'s own pubkey is also always a member (the engine discovers its
    // own write relays too -- see the headline test's note); this asserts
    // A and B are BOTH covered, not that the set is exactly `{A, B}`.
    let widened_authors = widened
        .authors
        .as_ref()
        .expect("discovery atom has authors");
    assert!(
        widened_authors.contains(&a.public_key().to_hex()),
        "A must still be covered after widening"
    );
    assert!(
        widened_authors.contains(&b.public_key().to_hex()),
        "B must now also be covered"
    );

    let sub_id_after = plan
        .sub_id_for_kind(&indexer, 10_002)
        .expect("discovery REQ must still exist after widening");
    assert_eq!(
        sub_id_first, sub_id_after,
        "widening the discovery author set must reuse the SAME sub-id (an overwriting \
         REQ), never close+reopen"
    );
}
