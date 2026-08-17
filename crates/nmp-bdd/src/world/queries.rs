//! The fixture query catalog: one constructor per SHAPE a scenario is
//! allowed to name ("my follows' notes", "notes tagged p as alice", "the
//! group state of every group I administer").
//!
//! Its own module because these are pure functions of their arguments -- no
//! `NmpWorld` state, no I/O, nothing to start -- and because the shape is the
//! thing under specification. A scenario's prose maps to exactly one of these
//! and to nothing else, so the set of shapes this suite can express is
//! readable in one file rather than inferred from the `When` steps that
//! happen to build one.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::LiveQuery;
use nmp_grammar::{IndexedTagName, ReadRouting};
use nmp_grammar::{Binding, Demand, Derived, Filter, IdentityField, Selector};
use nmp_router::RelayUrl;

/// NIP-29 group admins -- the inner query's kind: which groups name me.
pub(super) const GROUP_ADMINS_KIND: u16 = 39_001;
/// NIP-29 group metadata/admins/members -- the outer query's kinds.
const GROUP_STATE_KINDS: [u16; 3] = [39_000, 39_001, 39_002];

/// The `$myFollows` shape ("my follows' notes") -- the one feed shape the
/// starter catalog names (approach doc §2.4). Identical in structure to
/// `nmp`'s runtime-integration fixture query.
pub fn my_follows_query() -> LiveQuery {
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(std::collections::BTreeSet::from([1u16])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: Demand {
                    selection: Filter {
                        kinds: Some(std::collections::BTreeSet::from([3u16])),
                        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                        ..Filter::default()
                    },
                    ..Demand::default()
                },
                project: Selector::Tag("p".to_string()),
            }))),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

/// What makes one tag watch different from another beyond its value -- the
/// two shapes that must BLOCK a merge the value alone would allow.
///
/// A `limit` caps the relay-side RESULT COUNT rather than the predicate, so
/// two limited watches for different values cannot be unioned without
/// under-fetching. A `since` is a co-pinned time bound, and no union of two
/// windows both widens and stays near either operand. Either one present and
/// differing means the two watches must reach the relay as two subscriptions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WatchShape {
    pub limit: Option<usize>,
    pub since: Option<u64>,
}

/// `kinds:[1]` narrowed to ONE value of ONE single-letter tag and PINNED to
/// `relay` -- the smallest demand that exercises the tag axis of wire
/// subscription aggregation. `Explicit` rather than outbox-routed on purpose: the
/// contract under specification is about what reaches a NAMED relay, so the
/// scenario must not also depend on relay discovery choosing that relay.
pub fn tagged_note_query(relay: &RelayUrl, tag: char, value: &str, shape: WatchShape) -> LiveQuery {
    tagged_note_query_values(relay, tag, BTreeSet::from([value.to_string()]), shape)
}

/// The same shape carrying MORE THAN ONE value of the tag -- one app watch
/// that already asks for a set, which is what the scale watch in
/// [`super::watches`] opens instead of one handle per value (#994).
///
/// It is the same demand a merged pair would produce, so it does not weaken
/// what the collapse scenario can falsify: the coalescer's job is to reach one
/// bounded wire subscription regardless of how the app happened to split its
/// asks.
pub(super) fn tagged_note_query_values(
    relay: &RelayUrl,
    tag: char,
    values: BTreeSet<String>,
    shape: WatchShape,
) -> LiveQuery {
    let tag = IndexedTagName::new(tag).expect("nmp-bdd: an indexed tag name is one ASCII letter");
    pinned_query(
        relay,
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            tags: BTreeMap::from([(tag, Binding::Literal(values))]),
            limit: shape.limit,
            since: shape.since,
            ..Filter::default()
        },
    )
}

/// The same shape on the AUTHOR axis -- the one axis that already
/// aggregates, so every tag-axis scenario has a control to be measured
/// against. `limit` is what makes a pair of these UNMERGEABLE: a relay-side
/// `limit` caps the result COUNT, so the union refuses to widen across
/// one (see `nmp_router::coalesce::neither_limited`).
pub fn authored_note_query(relay: &RelayUrl, author_hex: &str, limit: Option<usize>) -> LiveQuery {
    authored_note_query_from_relays(BTreeSet::from([relay.clone()]), author_hex, limit)
}

/// One literal author's notes pinned to every relay in `relays`.
///
/// This is deliberately different from `my_follows_query`: the provenance
/// falsifier needs both named relays to be contacted, while outbox routing is
/// allowed to select a bounded covering subset of its candidates.
pub(super) fn authored_note_query_from_relays(
    relays: BTreeSet<RelayUrl>,
    author_hex: &str,
    limit: Option<usize>,
) -> LiveQuery {
    pinned_query_from_relays(
        relays,
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
            limit,
            ..Filter::default()
        },
    )
}

/// The shape a real group-first app hydrates with: "every group that lists
/// me as an admin" projected into the `#d` slot of "all state for those
/// groups". Identical in structure to `nmp-engine`'s own
/// `core_headless/derived_tag_fanout.rs` fixture, and the reason the tag axis
/// matters -- the resolved value set here is a CATALOG, not a handful.
///
/// `Explicit`, like the literal shapes above: group state has no author whose
/// outbox could be discovered, so a real client names its relay.
pub fn my_group_state_query(relay: &RelayUrl) -> LiveQuery {
    let pinned = BTreeSet::from([relay.clone()]);
    let inner = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([GROUP_ADMINS_KIND])),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').expect("'p' is an indexed tag name"),
                Binding::Reactive(IdentityField::ActivePubkey),
            )]),
            ..Filter::default()
        },
        ReadRouting::Explicit(pinned.clone().into_iter().collect()),
        None,
    )
    .expect("nmp-bdd: a pinned inner demand over a nonempty relay set is constructible");
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(GROUP_STATE_KINDS.into_iter().collect()),
                tags: BTreeMap::from([(
                    IndexedTagName::new('d').expect("'d' is an indexed tag name"),
                    Binding::Derived(Box::new(Derived {
                        inner,
                        project: Selector::Tag("d".to_string()),
                    })),
                )]),
                ..Filter::default()
            },
            ReadRouting::Explicit(pinned.into_iter().collect()),
            None,
        )
        .expect("nmp-bdd: a pinned outer demand over a nonempty relay set is constructible"),
    )
}

/// ONE group's metadata coordinate (`kind:39000`, `#d` = the group id) across
/// several named hosts, with NO author bound.
///
/// The unbound author is the whole shape: NIP-29 metadata is signed by the
/// host relay, so an author-scoped read could only ever return one host's
/// version and could not observe two hosts disagreeing. `Explicit`, like every
/// other literal shape here, because group state has no author whose outbox
/// could be discovered.
pub(super) fn group_metadata_query(relays: BTreeSet<RelayUrl>, group_id: &str) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([39_000u16])),
                tags: BTreeMap::from([(
                    IndexedTagName::new('d').expect("'d' is an indexed tag name"),
                    Binding::Literal(BTreeSet::from([group_id.to_string()])),
                )]),
                ..Filter::default()
            },
            ReadRouting::Explicit(relays.into_iter().collect()),
            None,
        )
        .expect("nmp-bdd: a pinned demand over a nonempty relay set is constructible"),
    )
}

/// One author's contact list -- the replaceable coordinate
/// `features/writes/replaceable-edits.feature` CAS-es against. `Explicit` like
/// every other literal shape: what the scenario reads is the LOCAL winner,
/// and naming the relay keeps the read from also depending on relay discovery.
pub fn contact_list_query(relay: &RelayUrl, author_hex: &str) -> LiveQuery {
    pinned_query(
        relay,
        Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
            ..Filter::default()
        },
    )
}

fn pinned_query(relay: &RelayUrl, filter: Filter) -> LiveQuery {
    pinned_query_from_relays(BTreeSet::from([relay.clone()]), filter)
}

fn pinned_query_from_relays(relays: BTreeSet<RelayUrl>, filter: Filter) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            filter,
            ReadRouting::Explicit(relays.into_iter().collect()),
            None,
        )
        .expect("nmp-bdd: a pinned demand over a nonempty relay set is constructible"),
    )
}
