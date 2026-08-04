//! [`GroupPredicate`] and [`GroupIds`] -- which groups a NIP-29 observation
//! covers (#1033, generalized by #1252).
//!
//! # Why these are not just a `Binding`
//!
//! A NIP-29 discovery predicate is an unfinished thing: it says "groups whose
//! member-list evidence names me" WITHOUT yet saying at which relay that
//! evidence was observed. It cannot say so, because the same predicate is
//! lowered once per host in the scope, each time against that host and no
//! other. A finished `Binding` has already answered that question and answered
//! it once; using one as the currency would force either a flattened
//! `Pinned({A, B})` -- which cross-products evidence observed at A onto a
//! listing at B, a confidently wrong answer -- or a recursive repin of a
//! caller's own binding, which is the silent inheritance `nmp_grammar::Derived`
//! exists to forbid.
//!
//! So the predicate stays open until a host is known, and the scope closes it
//! once per branch. That is the whole justification for these types; they own
//! no other property and add no grammar.
//!
//! # The predicate parameter IS the query language
//!
//! [`groups_whose_record_matches`] takes an ordinary [`Filter`] -- the same
//! `Filter` a live query takes -- and everything else that names ids is a
//! shorthand over it or over an ordinary [`Binding`]:
//!
//! ```text
//! member_list_includes(x)  ==  groups_whose_record_matches({ kinds:[39002], #p: x })
//! admin_list_includes(x)   ==  groups_whose_record_matches({ kinds:[39001], #p: x })
//! any_of(b)                ==  the `#d` row bound to `b`, verbatim
//! all()                    ==  NO `#d` row at all
//! ```
//!
//! The two `_list_includes` shorthands exist because they name protocol facts
//! honestly and are the common cases, NOT because the general spelling is
//! unavailable. `a_named_shorthand_is_exactly_the_general_spelling` asserts
//! the first two equalities rather than leaving them as prose.
//!
//! # Two axes, two types
//!
//! [`GroupPredicate`] answers "which groups": every group the host advertises,
//! or the groups a [`GroupIds`] names. [`GroupIds`] answers "where do the ids
//! come from": a query at the branch host, a caller-owned binding, or set
//! algebra over those.
//!
//! The split is not decoration. Set algebra is defined on [`GroupIds`] and on
//! nothing else, which makes `all().minus(...)` UNSPELLABLE rather than
//! refused at runtime. That is deliberate: Nostr filters have no negation, so
//! "everything except X" cannot narrow a wire request. It could only be
//! honoured by asking the relay for everything and hiding rows after
//! delivery -- the same spelling as every other `minus` with none of the
//! wire effect, which is the kind of quiet difference this codebase does not
//! ship. An app that wants to hide muted rooms from a directory drops them
//! from the `Vec<GroupSnapshot>` it renders, where the cost is visible.
//!
//! # Evidence, never exact state
//!
//! kind:39002 is an optional, possibly partial member-list snapshot;
//! kind:39001 an optional informative admin list. Inclusion is evidence;
//! ABSENCE IS NOT evidence of non-membership or of not being an admin. Hence
//! `member_list_includes` / `admin_list_includes` and no spelling that claims
//! exact current state. Reconstructing exact state from the canonical
//! kind:9000/9001 sequence is a different problem, deliberately not smuggled
//! in here.
//!
//! # Advertisement is not enumeration
//!
//! [`all`] reads what a host ADVERTISES. A relay that hosts a group and
//! publishes no kind:39000 for it is invisible to it, and nothing here claims
//! otherwise -- the same evidence discipline the list leaves carry, applied
//! to the metadata record.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{Binding, Filter, IndexedTagName, SetAlgebra, SetOp};
use nmp_nip29::{GroupRecord, GROUP_ADMINS_KIND, GROUP_MEMBERS_KIND};
use nostr::RelayUrl;

/// Which groups an observation covers.
///
/// Either every group the host advertises among the selected records, or the
/// groups some [`GroupIds`] names. A [`GroupIds`] converts into one, so an
/// app hands `member_list_includes(...)` straight to the observe door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPredicate {
    covers: Covers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Covers {
    /// No `#d` row on the branch demand at all.
    All,
    /// The `#d` row bound to what this resolves to.
    These(GroupIds),
}

/// Where a set of NIP-29 group ids comes from.
///
/// Composes with the grammar's own [`SetAlgebra`] through [`Self::union`],
/// [`Self::intersect`] and [`Self::minus`], so "a member here AND an admin
/// there", or "the groups that list me, plus the handful I pinned", costs
/// nothing extra. Lowering a composite lowers every leaf at the same host and
/// folds the results into an ordinary [`Binding::SetOp`].
///
/// # The `#d` wire limit is real and is not hidden
///
/// Whatever this resolves to becomes the `#d` value set of one relay filter,
/// and a filter carrying very many values may be refused or silently
/// truncated by that relay. That is true of a literal id list, and it is
/// equally true of a [`Binding::Derived`] or of a
/// [`groups_whose_record_matches`] leaf that happens to resolve to thousands
/// of ids -- a derived source is another spelling of the same hazard, not a
/// new one, and it is the hazard `member_list_includes` already had against a
/// relay that lists one subject in very many groups.
///
/// Watching very many groups at once needs sharding across several
/// observations. NMP does not hide that by chunking behind the app's back,
/// because a silently-sharded observation would report availability for a
/// plan the app never declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupIds {
    source: IdSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IdSource {
    /// The `d` values of the relay-signed group records matching this
    /// selection AT the branch host.
    AtHost(Filter),
    /// Whatever a caller-owned binding resolves to, embedded verbatim.
    Given(Binding),
    /// The grammar's own set algebra over host-open operands.
    SetOp {
        op: SetAlgebra,
        operands: Vec<GroupIds>,
    },
}

/// Why a selection could not become a [`GroupIds`].
///
/// Both variants are constructed in [`groups_whose_record_matches`] and
/// nowhere else: it is the ONLY door through which an app-written filter is
/// evaluated with NIP-29's own authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupPredicateError {
    /// The selection named no kind. It would be asked of the group's host
    /// with NIP-29's own pin, matching every event that relay holds, and its
    /// `d` values would then key the listing -- an unbounded question about
    /// data the host is not authoritative for.
    NoKindSelected,
    /// The selection named a kind that is not one of NIP-29's three
    /// relay-signed group records.
    ///
    /// This leaf is evaluated AT the group's host, pinned and
    /// cache-strict. Those three kinds are the only ones a group host is
    /// authoritative for, so a selection naming anything else would ask the
    /// wrong relay and silently under-resolve. An app whose ids come from its
    /// OWN data -- a kind:10009 simple-groups list, say -- passes that
    /// through [`any_of`] as a [`Binding::Derived`] carrying its own
    /// authority, which is never rewritten.
    NotAGroupRecordKind {
        /// The offending kind.
        kind: u16,
    },
}

impl std::fmt::Display for GroupPredicateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKindSelected => f.write_str(
                "a group-record selection must name at least one of NIP-29's three relay-signed \
                 group record kinds",
            ),
            Self::NotAGroupRecordKind { kind } => write!(
                f,
                "kind:{kind} is not one of NIP-29's three relay-signed group records; a group \
                 host is not authoritative for it"
            ),
        }
    }
}

impl std::error::Error for GroupPredicateError {}

/// Every group the host advertises among the selected records.
///
/// The branch demand carries NO `#d` row: this is the ABSENCE of a
/// constraint, not a constraint that happens to name everything. That is what
/// makes a directory expressible at all -- the ids a directory wants are the
/// answer, not the input.
///
/// # Unbounded by nature
///
/// Every other spelling is self-limiting: a membership question is bounded by
/// who is listed, an id set by its own members. This one names nothing, so
/// what comes back is whatever the relay chooses to answer with. The bound is
/// the observation's own per-host `limit`
/// ([`RelayScope::observe`](super::RelayScope::observe)'s fourth argument),
/// which is the ordinary NIP-01 `Filter::limit` every other read uses --
/// there is no `all`-specific knob, because unboundedness is not
/// `all`-specific either.
///
/// # Advertisement is not enumeration
///
/// A kind:39000 read establishes that the host advertises a group. A group
/// the host serves but publishes no kind:39000 for is invisible, and no
/// completeness is claimed over what comes back.
#[must_use]
pub fn all() -> GroupPredicate {
    GroupPredicate {
        covers: Covers::All,
    }
}

/// Groups whose own relay-signed record matches `selection` at the branch
/// host -- THE general spelling, of which every other id source below is a
/// shorthand.
///
/// `selection` is an ordinary [`Filter`]: anything a live query can express
/// over one of NIP-29's three relay-signed records, a group observation can
/// express here. The `d` values of the matching records are what key the
/// listing, because `d` is the join key NIP-29 itself defines between the
/// three records -- that projection is a protocol fact and is not a caller
/// choice.
///
/// Fallible for exactly one reason: this selection is evaluated with NIP-29's
/// OWN authority (pinned to the branch host, `CacheMode::Strict`), so it must
/// name only records that host is authoritative for. See
/// [`GroupPredicateError`].
///
/// The selection's `kinds` are the INNER question and have nothing to do with
/// the [`GroupRecord`]s the observation renders: "which groups list me as a
/// member" (kind:39002) and "show me their metadata" (kind:39000) is the
/// ordinary composition, not a conflict.
pub fn groups_whose_record_matches(selection: Filter) -> Result<GroupIds, GroupPredicateError> {
    let kinds = selection
        .kinds
        .as_ref()
        .ok_or(GroupPredicateError::NoKindSelected)?;
    if kinds.is_empty() {
        return Err(GroupPredicateError::NoKindSelected);
    }
    for kind in kinds {
        if GroupRecord::of_kind(*kind).is_none() {
            return Err(GroupPredicateError::NotAGroupRecordKind { kind: *kind });
        }
    }
    Ok(GroupIds {
        source: IdSource::AtHost(selection),
    })
}

/// Groups whose observed member-list evidence names `subjects`.
///
/// Shorthand for `groups_whose_record_matches({ kinds:[39002], #p: subjects })`,
/// and exactly equal to it.
///
/// `subjects` is an ordinary [`Binding`], so
/// `Binding::Reactive(IdentityField::ActivePubkey)` stays REACTIVE and the
/// query follows the logged-in user across an account switch -- nothing here
/// flattens it to a literal. A `subjects` binding that is itself derived
/// (say, a kind:3 follows lookup resolving from the author's own outboxes)
/// keeps its OWN complete authority; lowering never rewrites it.
#[must_use]
pub fn member_list_includes(subjects: Binding) -> GroupIds {
    list_evidence(GROUP_MEMBERS_KIND, subjects)
}

/// Groups whose observed admin-list evidence names `subjects`.
///
/// Shorthand for `groups_whose_record_matches({ kinds:[39001], #p: subjects })`,
/// and exactly equal to it. Evidence-scoped exactly like
/// [`member_list_includes`].
#[must_use]
pub fn admin_list_includes(subjects: Binding) -> GroupIds {
    list_evidence(GROUP_ADMINS_KIND, subjects)
}

fn list_evidence(kind: u16, subjects: Binding) -> GroupIds {
    let selection = Filter {
        kinds: Some(BTreeSet::from([kind])),
        tags: BTreeMap::from([(subject_tag(), subjects)]),
        ..Filter::default()
    };
    groups_whose_record_matches(selection)
        .expect("a NIP-29-owned list kind is one of the three relay-signed group records")
}

/// The groups `ids` names, whatever any list says about them.
///
/// `ids` is an ordinary [`Binding`], which is the whole point: a literal set
/// for rooms an app already knows, and a [`Binding::Derived`] for rooms it
/// has to look up. "Watch the groups named in my own kind:10009 simple-groups
/// list" is that derived case, and it stays REACTIVE -- when the list
/// changes, the observation follows it. No hand-extraction of ids, no second
/// observation, no re-opening anything.
///
/// The binding is embedded VERBATIM. A derived one keeps its own complete
/// authority -- an app's kind:10009 lookup resolves from the app's own
/// relays, never from the group's hosts, and lowering never repins it.
#[must_use]
pub fn any_of(ids: Binding) -> GroupIds {
    GroupIds {
        source: IdSource::Given(ids),
    }
}

impl GroupIds {
    /// Groups named by this source OR by any of `others`.
    #[must_use]
    pub fn union(self, others: impl IntoIterator<Item = GroupIds>) -> Self {
        self.combine(SetAlgebra::Union, others)
    }

    /// Groups named by this source AND by all of `others`.
    #[must_use]
    pub fn intersect(self, others: impl IntoIterator<Item = GroupIds>) -> Self {
        self.combine(SetAlgebra::Intersect, others)
    }

    /// Groups named by this source and by none of `others`.
    #[must_use]
    pub fn minus(self, others: impl IntoIterator<Item = GroupIds>) -> Self {
        self.combine(SetAlgebra::Diff, others)
    }

    fn combine(self, op: SetAlgebra, others: impl IntoIterator<Item = GroupIds>) -> Self {
        let mut operands = vec![self];
        operands.extend(others);
        Self {
            source: IdSource::SetOp { op, operands },
        }
    }

    /// Close this source against ONE host: every NIP-29-owned level in the
    /// result is pinned to exactly this host, and every caller-supplied
    /// binding inside it survives untouched.
    fn lower_at(&self, host: &RelayUrl) -> Binding {
        match &self.source {
            IdSource::AtHost(selection) => {
                nmp_nip29::records_matching_at(host, selection.clone())
            }
            // A caller's binding names values under its own authority: there
            // is nothing here to pin and nothing here NMP may repin.
            IdSource::Given(ids) => ids.clone(),
            IdSource::SetOp { op, operands } => Binding::SetOp(Box::new(SetOp {
                op: *op,
                operands: operands.iter().map(|each| each.lower_at(host)).collect(),
            })),
        }
    }
}

impl From<GroupIds> for GroupPredicate {
    fn from(ids: GroupIds) -> Self {
        Self {
            covers: Covers::These(ids),
        }
    }
}

impl GroupPredicate {
    /// What the branch demand's `#d` row is bound to at `host`, or `None`
    /// when there is to be no `#d` row at all.
    pub(crate) fn lower_at(&self, host: &RelayUrl) -> Option<Binding> {
        match &self.covers {
            Covers::All => None,
            Covers::These(ids) => Some(ids.lower_at(host)),
        }
    }
}

fn subject_tag() -> IndexedTagName {
    IndexedTagName::new('p').expect("'p' is a single ASCII letter")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use nmp_grammar::{
        AccessContext, Demand, Derived, IdentityField, IndexedTagName, Selector, SourceAuthority,
    };

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    fn pinned(host: RelayUrl) -> SourceAuthority {
        SourceAuthority::Pinned(BTreeSet::from([host]))
    }

    fn me() -> Binding {
        Binding::Reactive(IdentityField::ActivePubkey)
    }

    fn derived(binding: &Binding) -> &Derived {
        match binding {
            Binding::Derived(inner) => inner,
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    fn lowered(ids: GroupIds, at: &RelayUrl) -> Binding {
        GroupPredicate::from(ids)
            .lower_at(at)
            .expect("an id source always binds #d")
    }

    /// A follows lookup an APP built: its own kind:3 selection resolving from
    /// the author's own outboxes, which no NIP-29 layer may repin.
    fn follows_of_me() -> Binding {
        Binding::Derived(Box::new(Derived {
            inner: Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(me()),
                    ..Filter::default()
                },
                SourceAuthority::AuthorOutboxes,
                AccessContext::Public,
            )
            .expect("an author-bound outbox demand is constructible"),
            project: Selector::Tag("p".to_string()),
        }))
    }

    /// An app's OWN kind:10009 simple-groups list, resolving from the app's
    /// own relays: the shape `any_of` could not take before #1252.
    fn my_saved_group_list() -> Binding {
        Binding::Derived(Box::new(Derived {
            inner: Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([10009u16])),
                    authors: Some(me()),
                    ..Filter::default()
                },
                SourceAuthority::AuthorOutboxes,
                AccessContext::Public,
            )
            .expect("an author-bound outbox demand is constructible"),
            project: Selector::Tag("group".to_string()),
        }))
    }

    #[test]
    fn a_lowered_member_predicate_pins_its_own_level_to_the_exact_host() {
        let lowered = lowered(member_list_includes(me()), &host(1));
        assert_eq!(derived(&lowered).inner.source, pinned(host(1)));
        assert_eq!(
            derived(&lowered).inner.selection.kinds,
            Some(BTreeSet::from([39002u16]))
        );
    }

    /// The identity reference survives lowering as a REACTIVE binding, so an
    /// account switch re-resolves the query instead of pinning yesterday's
    /// user into it.
    #[test]
    fn the_active_pubkey_stays_reactive_through_lowering() {
        let lowered = lowered(member_list_includes(me()), &host(1));
        let p = IndexedTagName::new('p').unwrap();
        assert_eq!(derived(&lowered).inner.selection.tags.get(&p), Some(&me()));
    }

    /// The named leaves are SHORTHANDS, not a second vocabulary: each is
    /// exactly the general spelling with its filter written out. Assert it
    /// rather than claim it, so a leaf that quietly grows its own lowering
    /// path fails here.
    #[test]
    fn a_named_shorthand_is_exactly_the_general_spelling() {
        for (shorthand, kind) in [
            (member_list_includes(me()), 39002u16),
            (admin_list_includes(me()), 39001u16),
        ] {
            let general = groups_whose_record_matches(Filter {
                kinds: Some(BTreeSet::from([kind])),
                tags: BTreeMap::from([(IndexedTagName::new('p').unwrap(), me())]),
                ..Filter::default()
            })
            .expect("a relay-signed group record kind is accepted");
            assert_eq!(
                shorthand, general,
                "the kind:{kind} shorthand must BE the general spelling, not merely resemble it"
            );
        }
    }

    /// The general spelling reaches a question no shorthand names: "groups
    /// whose metadata record carries this `name` row". Nothing about the
    /// predicate is limited to `#p` or to the two list kinds any more.
    #[test]
    fn the_general_spelling_constrains_a_field_no_shorthand_names() {
        let ids = groups_whose_record_matches(Filter {
            kinds: Some(BTreeSet::from([39000u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('t').unwrap(),
                Binding::Literal(BTreeSet::from(["photography".to_string()])),
            )]),
            ..Filter::default()
        })
        .expect("kind:39000 is a relay-signed group record");
        let lowered = lowered(ids, &host(1));
        let inner = &derived(&lowered).inner;
        assert_eq!(inner.selection.kinds, Some(BTreeSet::from([39000u16])));
        assert_eq!(
            inner.selection.tags.get(&IndexedTagName::new('t').unwrap()),
            Some(&Binding::Literal(BTreeSet::from([
                "photography".to_string()
            ])))
        );
        assert_eq!(inner.source, pinned(host(1)));
    }

    /// The host-evaluated leaf is evaluated with NIP-29's OWN pin, so it may
    /// name only records the group's host is authoritative for. A kind the
    /// host has no reason to hold is refused at construction, not asked for
    /// and silently under-resolved.
    #[test]
    fn a_kind_the_group_host_is_not_authoritative_for_is_refused() {
        assert_eq!(
            groups_whose_record_matches(Filter {
                kinds: Some(BTreeSet::from([10009u16])),
                ..Filter::default()
            })
            .err(),
            Some(GroupPredicateError::NotAGroupRecordKind { kind: 10009 })
        );
        assert_eq!(
            groups_whose_record_matches(Filter::default()).err(),
            Some(GroupPredicateError::NoKindSelected)
        );
        assert_eq!(
            groups_whose_record_matches(Filter {
                kinds: Some(BTreeSet::new()),
                ..Filter::default()
            })
            .err(),
            Some(GroupPredicateError::NoKindSelected)
        );
    }

    /// The capability #1252 names: an app's OWN saved-groups list drives the
    /// observation as a DERIVED binding, keeping its own authority. Before
    /// this the id set had to be a literal, so the app extracted ids by hand
    /// and re-opened the observation whenever its list changed.
    #[test]
    fn a_derived_id_source_keeps_its_own_authority_and_stays_reactive() {
        let lowered = lowered(any_of(my_saved_group_list()), &host(1));
        assert_eq!(
            derived(&lowered).inner.source,
            SourceAuthority::AuthorOutboxes,
            "the app's own list resolves from the app's own relays, never from the group's host"
        );
        assert_eq!(
            derived(&lowered).inner.selection.kinds,
            Some(BTreeSet::from([10009u16]))
        );
        assert_eq!(
            derived(&lowered).inner.selection.authors,
            Some(me()),
            "the list lookup stays reactive, so an account switch re-resolves it"
        );
    }

    /// Composition is the grammar's own set algebra over ordinary bindings --
    /// no conversion, no second combinator vocabulary.
    #[test]
    fn set_algebra_composes_id_sources_into_ordinary_bindings() {
        let composed = lowered(
            member_list_includes(me()).intersect([admin_list_includes(follows_of_me())]),
            &host(1),
        );
        match composed {
            Binding::SetOp(set) => {
                assert_eq!(set.op, SetAlgebra::Intersect);
                assert_eq!(set.operands.len(), 2);
                for operand in &set.operands {
                    assert_eq!(
                        derived(operand).inner.source,
                        pinned(host(1)),
                        "every NIP-29-owned level is pinned to the branch host"
                    );
                }
            }
            other => panic!("expected SetOp, got {other:?}"),
        }
    }

    /// THE hazard this whole design exists to prevent, stated as an
    /// assertion: a NIP-29-owned level is pinned to the branch host, and a
    /// caller-owned level nested inside it keeps its own authority. Inherit
    /// or blanket-repin, and the innermost kind:3 lookup would ask the
    /// group's hosts for a contact list they have no reason to hold -- it
    /// would not error, it would silently under-resolve.
    #[test]
    fn a_caller_owned_inner_demand_keeps_its_own_authority_at_depth_two() {
        let lowered = lowered(admin_list_includes(follows_of_me()), &host(1));

        // Depth 1: NIP-29's own admin-record lookup, pinned to the host.
        let admin_records = &derived(&lowered).inner;
        assert_eq!(admin_records.source, pinned(host(1)));

        // Depth 2: the app's follows lookup, still on the author's outboxes.
        let p = IndexedTagName::new('p').unwrap();
        let subjects = admin_records
            .selection
            .tags
            .get(&p)
            .expect("the admin lookup binds #p to the caller's subjects");
        assert_eq!(
            derived(subjects).inner.source,
            SourceAuthority::AuthorOutboxes,
            "NIP-29 never recursively overwrites app-owned authority"
        );
    }

    /// Lowering the same id source at two hosts yields two independent
    /// values, each closed against its own host and neither mentioning the
    /// other.
    #[test]
    fn two_hosts_lower_to_two_independently_pinned_values() {
        let ids = member_list_includes(me());
        let at_one = lowered(ids.clone(), &host(1));
        let at_two = lowered(ids, &host(2));
        assert_ne!(at_one, at_two);
        assert_eq!(derived(&at_one).inner.source, pinned(host(1)));
        assert_eq!(derived(&at_two).inner.source, pinned(host(2)));
    }

    /// A known-id watch needs no relational question and no subject: the ids
    /// lower to the literal `#d` set the wire already takes.
    #[test]
    fn a_literal_id_set_lowers_to_the_d_values_themselves() {
        let literal = Binding::Literal(BTreeSet::from([
            "photographers".to_string(),
            "darkroom".to_string(),
        ]));
        assert_eq!(lowered(any_of(literal.clone()), &host(1)), literal);
        assert_eq!(
            lowered(any_of(literal.clone()), &host(2)),
            literal,
            "a literal names values and resolves nothing, so it is the same at every host"
        );
    }

    /// The shape a real watch list has: reactive membership evidence UNION a
    /// handful of pinned ids, in one predicate, lowered once per host.
    #[test]
    fn pinned_ids_compose_with_evidence_in_one_predicate() {
        let pinned_ids = Binding::Literal(BTreeSet::from(["photographers".to_string()]));
        let lowered = lowered(
            member_list_includes(me()).union([any_of(pinned_ids.clone())]),
            &host(1),
        );
        match lowered {
            Binding::SetOp(set) => {
                assert_eq!(set.op, SetAlgebra::Union);
                assert_eq!(set.operands.len(), 2);
                assert_eq!(derived(&set.operands[0]).inner.source, pinned(host(1)));
                assert_eq!(set.operands[1], pinned_ids);
            }
            other => panic!("expected SetOp, got {other:?}"),
        }
    }

    #[test]
    fn union_and_diff_fold_with_the_grammars_own_algebra() {
        for (ids, expected) in [
            (
                member_list_includes(me()).union([admin_list_includes(me())]),
                SetAlgebra::Union,
            ),
            (
                member_list_includes(me()).minus([admin_list_includes(me())]),
                SetAlgebra::Diff,
            ),
        ] {
            match lowered(ids, &host(1)) {
                Binding::SetOp(set) => assert_eq!(set.op, expected),
                other => panic!("expected SetOp, got {other:?}"),
            }
        }
    }

    /// THE #1252 falsifier. "Every group this relay hosts" is the ABSENCE of
    /// a `#d` constraint. Lower it to anything that still binds `#d` and the
    /// relay is asked about a specific id set -- for a directory, whichever
    /// ids the app already knew, which is indistinguishable from a host with
    /// no groups at all.
    #[test]
    fn all_binds_no_group_id_row_whatsoever() {
        assert_eq!(all().lower_at(&host(1)), None);
        assert_eq!(all().lower_at(&host(2)), None);
    }
}
