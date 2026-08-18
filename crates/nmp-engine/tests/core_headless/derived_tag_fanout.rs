//! Empirical study: how a `Binding::Derived` in a TAG slot recompiles onto
//! the wire as its resolved set grows.
//!
//! The shape under study (mosaico's real group-state hydration query):
//!
//! ```text
//! kinds:[39000,39001,39002], #d := Derived(
//!     inner   = kinds:[39001], #p:[$activePubkey],
//!     project = Tag("d"),
//! )
//! ```
//!
//! "which groups am I an admin of" → "hydrate all group state for those
//! groups". Every measurement here is a real `EngineCore` run: zero I/O, but
//! the same `EngineMsg::RelayFrame` ingest path a live relay drives.
//!
//! THE LEDGERS BELOW WERE INVERTED when `nmp_router::StructuralUnion` landed.
//! They originally measured the fan-out as a defect: N resolved `#d` values
//! opened N wire subscriptions carrying one value each, while the SAME
//! derived binding in the `authors` slot collapsed onto one. Both halves of
//! that asymmetry are gone -- the merge rule folds the per-value atoms
//! (#900/§7.1). A byte-changing widened filter now receives a fresh token;
//! exact local acceptance closes its predecessor, so the steady state remains
//! one current request per relay without same-id overwrite (#774).
//!
//! Run the narrated ledgers with:
//! `cargo test -p nmp --test core_headless derived_tag_fanout -- --nocapture`

use std::collections::BTreeMap;

use super::*;

const OUTER_KINDS: [u16; 3] = [39_000, 39_001, 39_002];
const INNER_KIND: u16 = 39_001;

// ---- fixtures -----------------------------------------------------------

/// A kind:39001 (NIP-29 group admins) event: `group` as the `d` tag,
/// `admins` as `p` tags. Mirrors `nmp_resolver_testkit::kind39002`, which
/// covers 39002 (members) but not 39001.
fn group_admins(
    author: &Keys,
    group: &str,
    admins: &[nostr::PublicKey],
    created_at: u64,
) -> nostr::Event {
    let mut tags = vec![nostr::Tag::identifier(group)];
    tags.extend(admins.iter().map(|pk| nostr::Tag::public_key(*pk)));
    nostr::EventBuilder::new(Kind::from(INNER_KIND), "")
        .tags(tags)
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// The query under study, scoped to `relays` — mosaico reaches the wire via
/// `ReadRouting::Explicit`, never via outbox routing, so the study must too
/// or the outer atom never routes at all.
fn group_state_of_my_admin_groups(relays: &[RelayUrl]) -> LiveQuery {
    let pinned: BTreeSet<RelayUrl> = relays.iter().cloned().collect();
    let inner = nmp_grammar::Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([INNER_KIND])),
            tags: BTreeMap::from([(
                nmp_grammar::IndexedTagName::new('p').unwrap(),
                Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey),
            )]),
            ..Filter::default()
        },
        ReadRouting::Explicit(pinned.clone().into_iter().collect()),
    )
    .expect("pinned inner demand with a nonempty relay set is constructible");
    let outer = Filter {
        kinds: Some(OUTER_KINDS.iter().copied().collect()),
        tags: BTreeMap::from([(
            nmp_grammar::IndexedTagName::new('d').unwrap(),
            Binding::Derived(Box::new(nmp_grammar::Derived {
                inner,
                project: nmp_grammar::Selector::Tag("d".to_string()),
            })),
        )]),
        ..Filter::default()
    };
    LiveQuery::single(
        nmp_grammar::Demand::new(outer, ReadRouting::Explicit(pinned.into_iter().collect()))
            .expect("pinned outer demand with a nonempty relay set is constructible"),
    )
}

// ---- wire measurement ---------------------------------------------------

/// Every `WireOp` in `effects`, flattened across relays, in order.
fn wire_ops(effects: &[Effect]) -> Vec<(&RelaySessionKey, &WireOp)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta.ops.iter()),
            _ => None,
        })
        .flatten()
        .flat_map(|(session, ops)| ops.iter().map(move |op| (session, op)))
        .collect()
}

/// The `#d` values carried by a REQ filter, for readable ledgers.
fn d_values(filter: &ConcreteFilter) -> Vec<String> {
    filter
        .tags
        .get(&nmp_grammar::IndexedTagName::new('d').unwrap())
        .map(|set| set.iter().cloned().collect())
        .unwrap_or_default()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StepCount {
    /// REQs minting a sub-id not previously live — a genuinely new socket sub.
    opened: usize,
    /// REQs re-using a live sub-id. Byte-changing requests must keep this zero.
    replaced: usize,
    closed: usize,
}

/// Drive the same exact local-acceptance boundary as the runtime, then append
/// its effects after the offered REQs. A byte-changing successor is therefore
/// visible as `REQ(new)` followed by `CLOSE(old)`, never as a same-id rewrite.
fn accept_requests(
    core: &mut EngineCore,
    handles: &BTreeMap<RelaySessionKey, RelayHandle>,
    offered: &[Effect],
) -> Vec<Effect> {
    let attempts = offered
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| {
            delta.ops.iter().flat_map(move |(session, ops)| {
                ops.iter().filter_map(move |op| match op {
                    WireOp::Req(sub_id, filter) => {
                        Some((delta.attempt_id(session, sub_id, filter), handles[session]))
                    }
                    WireOp::Close(_) => None,
                })
            })
        })
        .collect::<Vec<_>>();

    attempts
        .into_iter()
        .flat_map(|(attempt_id, handle)| {
            core.on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle })
        })
        .collect()
}

fn record_accepted_step(
    core: &mut EngineCore,
    handles: &BTreeMap<RelaySessionKey, RelayHandle>,
    ledger: &mut WireLedger,
    label: &str,
    mut effects: Vec<Effect>,
) -> StepCount {
    let accepted = accept_requests(core, handles, &effects);
    effects.extend(accepted);
    ledger.record(label, &effects)
}

/// Tracks live wire subscriptions and cumulative wire traffic across a run,
/// distinguishing the two costs that pull in opposite directions:
/// steady-state sub count vs cumulative wire messages (churn).
#[derive(Default)]
struct WireLedger {
    live: BTreeMap<SubId, ConcreteFilter>,
    steps: Vec<(String, StepCount)>,
    total: StepCount,
    /// The kinds identifying this study's OUTER query, so the ledger reports
    /// the right subs when a scenario uses a different shape.
    outer_kinds: BTreeSet<u16>,
}

impl WireLedger {
    fn for_kinds(kinds: &[u16]) -> Self {
        Self {
            outer_kinds: kinds.iter().copied().collect(),
            ..Self::default()
        }
    }

    fn is_outer(&self, filter: &ConcreteFilter) -> bool {
        filter.kinds.as_ref() == Some(&self.outer_kinds)
    }

    fn record(&mut self, label: &str, effects: &[Effect]) -> StepCount {
        let mut step = StepCount::default();
        for (_, op) in wire_ops(effects) {
            match op {
                WireOp::Req(id, filter) => {
                    if self.live.insert(id.clone(), filter.clone()).is_some() {
                        step.replaced += 1;
                    } else {
                        step.opened += 1;
                    }
                }
                WireOp::Close(id) => {
                    self.live.remove(id);
                    step.closed += 1;
                }
            }
        }
        self.total.opened += step.opened;
        self.total.replaced += step.replaced;
        self.total.closed += step.closed;
        self.steps.push((label.to_string(), step));
        step
    }

    /// Live subs whose filter is the OUTER shape, with their `#d` payloads.
    fn live_outer(&self) -> Vec<Vec<String>> {
        self.live
            .values()
            .filter(|f| self.is_outer(f))
            .map(d_values)
            .collect()
    }

    fn live_outer_count(&self) -> usize {
        self.live.values().filter(|f| self.is_outer(f)).count()
    }

    /// The widest value set any single live outer sub carries — 1 under
    /// fan-out, N under a widened filter.
    fn widest_outer(&self) -> usize {
        self.live
            .values()
            .filter(|f| self.is_outer(f))
            .map(|f| {
                f.authors.as_ref().map(|a| a.len()).unwrap_or(0).max(
                    f.tags
                        .get(&nmp_grammar::IndexedTagName::new('d').unwrap())
                        .map(|v| v.len())
                        .unwrap_or(0),
                )
            })
            .max()
            .unwrap_or(0)
    }

    fn report(&self, title: &str) {
        println!("\n=== {title} ===");
        println!(
            "{:<44} {:>6} {:>9} {:>7} {:>11}",
            "step", "opened", "replaced", "closed", "live(outer)"
        );
        let mut live_at = 0usize;
        for (label, step) in &self.steps {
            live_at = live_at + step.opened - step.closed;
            println!(
                "{:<44} {:>6} {:>9} {:>7} {:>11}",
                label, step.opened, step.replaced, step.closed, live_at
            );
        }
        println!(
            "{:<44} {:>6} {:>9} {:>7} {:>11}",
            "TOTAL", self.total.opened, self.total.replaced, self.total.closed, ""
        );
        println!(
            "live outer subs at end: {} → {:?}",
            self.live_outer_count(),
            self.live_outer()
        );
        println!(
            "cumulative wire messages (opened+replaced+closed): {}",
            self.total.opened + self.total.replaced + self.total.closed
        );
    }
}

// ---- scenario scaffolding ----------------------------------------------

struct Study {
    core: EngineCore,
    handles: BTreeMap<RelaySessionKey, RelayHandle>,
    me: Keys,
    group_author: Keys,
    ledger: WireLedger,
    clock: u64,
}

impl Study {
    /// A study with `relays` connected on ascending slots and an active
    /// pubkey set, but NOT yet subscribed.
    fn new(relays: &[RelayUrl]) -> Self {
        let mut core = new_core(FixtureRoutingFacts::new());
        let mut handles = BTreeMap::new();
        for (slot, relay) in relays.iter().enumerate() {
            connect(&mut core, slot as u32, relay);
            handles.insert(
                public_session(relay),
                RelayHandle {
                    slot: slot as u32,
                    generation: 1,
                },
            );
        }
        let me = Keys::generate();
        core.handle(EngineMsg::SetActivePubkey(Some(me.public_key())));
        Self {
            core,
            handles,
            me,
            group_author: Keys::generate(),
            ledger: WireLedger::for_kinds(&OUTER_KINDS),
            clock: 100,
        }
    }

    fn subscribe(&mut self, relays: &[RelayUrl], label: &str) -> StepCount {
        let effects = self
            .core
            .handle_and_flush(EngineMsg::Subscribe(group_state_of_my_admin_groups(relays)));
        record_accepted_step(
            &mut self.core,
            &self.handles,
            &mut self.ledger,
            label,
            effects,
        )
    }

    /// Deliver one kind:39001 naming `me` an admin of `group`, arriving on
    /// `relay`'s session under wire sub id `sub`.
    fn admin_of(
        &mut self,
        relay: &RelayUrl,
        slot: u32,
        sub: &str,
        group: &str,
        label: &str,
    ) -> StepCount {
        self.clock += 1;
        let event = group_admins(
            &self.group_author,
            group,
            &[self.me.public_key()],
            self.clock,
        );
        let effects = self.core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot,
                generation: 1,
            },
            public_session(relay),
            event_frame(sub, event),
        ));
        record_accepted_step(
            &mut self.core,
            &self.handles,
            &mut self.ledger,
            label,
            effects,
        )
    }

    /// Ingest without recording — used to warm the store BEFORE subscribing,
    /// where there is no subscription for wire ops to belong to.
    fn preload_admin_of(&mut self, relay: &RelayUrl, slot: u32, group: &str) {
        self.clock += 1;
        let event = group_admins(
            &self.group_author,
            group,
            &[self.me.public_key()],
            self.clock,
        );
        self.core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot,
                generation: 1,
            },
            public_session(relay),
            event_frame("preload", event),
        ));
    }

    fn eose(&mut self, relay: &RelayUrl, slot: u32, sub: &SubId, label: &str) -> StepCount {
        let effects = self.core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot,
                generation: 1,
            },
            public_session(relay),
            eose_frame(&wire_sub_string(sub)),
        ));
        record_accepted_step(
            &mut self.core,
            &self.handles,
            &mut self.ledger,
            label,
            effects,
        )
    }

    /// The live INNER sub-id (kinds:[39001]) — the one a relay EOSEs to
    /// declare "that's all the admin lists I have".
    fn inner_sub_id(&self) -> SubId {
        self.ledger
            .live
            .iter()
            .find(|(_, f)| {
                f.kinds == Some(BTreeSet::from([INNER_KIND]))
                    && f.tags
                        .contains_key(&nmp_grammar::IndexedTagName::new('p').unwrap())
            })
            .map(|(id, _)| id.clone())
            .expect("the inner query must be live on the wire")
    }
}

fn relay(n: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://relay{n}.example.com")).unwrap()
}

// ---- PROBE --------------------------------------------------------------

/// PROBE — the shape check the whole study rests on.
///
/// One admin group in the inner set must produce exactly one outer REQ,
/// carrying exactly that group's `#d`. If this is zero, the outer atom never
/// routed and every later measurement is measuring nothing.
#[test]
fn probe_single_inner_value_reaches_the_wire_as_one_req() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));

    study.subscribe(std::slice::from_ref(&r0), "subscribe (empty derived set)");
    assert_eq!(
        study.ledger.live_outer_count(),
        0,
        "an empty derived set must never widen to a wildcard outer REQ"
    );

    study.admin_of(&r0, 0, "s", "group-1", "admin of group-1");
    assert_eq!(study.ledger.live_outer(), vec![vec!["group-1".to_string()]]);
}

// ---- A. cold cache, incremental growth ---------------------------------

/// A — the headline measurement. Five admin groups arrive one at a time on a
/// live relay. How many outer REQs exist, and how much wire churn did
/// getting there cost?
///
/// INVERTED. This asserted `opened: 1` per value and five live outer subs
/// each carrying a singleton `#d` -- the fan-out. Each newly resolved value
/// is now a ONE-COMPONENT difference from the filter already live, so it is
/// coalesced into it and offered under a fresh token. Exact local acceptance
/// then closes the predecessor: `opened: 1`, `closed: 1`, and one current
/// outer sub whose `#d` array grows to five.
#[test]
fn a_incremental_growth_uses_fresh_successors_and_one_current_sub() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));
    study.subscribe(std::slice::from_ref(&r0), "subscribe");

    for n in 1..=5 {
        let group = format!("group-{n}");
        let step = study.admin_of(&r0, 0, "s", &group, &format!("admin of {group}"));
        let expected = if n == 1 {
            // The first value has nothing to widen: it opens the sub.
            StepCount {
                opened: 1,
                replaced: 0,
                closed: 0,
            }
        } else {
            StepCount {
                opened: 1,
                replaced: 0,
                closed: 1,
            }
        };
        assert_eq!(
            step, expected,
            "after the first value, each byte-changing request must open a fresh \
             successor and close its predecessor only after local acceptance"
        );
        assert_eq!(
            study.ledger.live_outer_count(),
            1,
            "the outer subscription count must not grow with the value set"
        );
        assert_eq!(study.ledger.widest_outer(), n, "the value set itself grows");
    }

    study
        .ledger
        .report("A. cold cache, 5 values arriving one at a time");

    assert_eq!(study.ledger.live_outer_count(), 1);
    assert_eq!(
        study.ledger.total.closed, 4,
        "each accepted byte-changing successor retires exactly one predecessor"
    );
    assert_eq!(
        study.ledger.total.opened, 6,
        "one inner sub plus five fresh outer generations were offered"
    );
    assert_eq!(study.ledger.total.replaced, 0);
    // ONE outer sub carrying all five #d values -- the collapse, measured.
    assert_eq!(
        study.ledger.live_outer(),
        vec![(1..=5).map(|n| format!("group-{n}")).collect::<Vec<_>>()],
        "one outer REQ carries every resolved #d value"
    );
}

// ---- B. warm cache ------------------------------------------------------

/// B — the user's "are cache reads served atomically?" question. Five admin
/// lists are already in the store when the subscription opens.
///
/// The single-recompile property always held; what changed is what that one
/// recompile emits. It opened five outer subs plus the inner one; it now
/// opens ONE outer sub carrying all five values, because the five atoms
/// coalesce before any of them reaches a token.
#[test]
fn b_warm_cache_resolves_the_whole_set_in_one_recompile() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));
    for n in 1..=5 {
        study.preload_admin_of(&r0, 0, &format!("group-{n}"));
    }

    let step = study.subscribe(
        std::slice::from_ref(&r0),
        "subscribe (5 values already cached)",
    );
    study
        .ledger
        .report("B. warm cache, 5 values resolved at subscribe time");

    assert_eq!(
        study.ledger.live_outer_count(),
        1,
        "a warm cache must resolve the full derived set into ONE outer sub"
    );
    assert_eq!(
        study.ledger.widest_outer(),
        5,
        "a warm cache must resolve the FULL derived set at subscribe time -- all \
         five values, in the one filter"
    );
    assert_eq!(
        step.opened, 2,
        "the one outer sub plus the inner sub open in ONE recompile — the cache \
         read is atomic"
    );
    assert_eq!(step.closed, 0);
    assert_eq!(step.replaced, 0);
}

/// B2 — THE CASE THE BDD SUITE FAILED ON, reduced to a deterministic ledger:
/// a WARM set resolved together, and then ONE more value arriving live.
///
/// A grows one at a time from cold and B resolves five at once, but nothing
/// covered the join between them, which is the shape a real app has
/// constantly: a catalog is already known, and then one more entry appears.
/// The feature-level contract exercises exactly this against a live engine.
///
/// What must hold is what A already proves for the cold path: the sixth value
/// is a ONE-COMPONENT difference from the live five-value filter, so it opens
/// under a fresh token; acceptance then retires the predecessor.
#[test]
fn b2_one_more_value_after_a_warm_set_uses_a_fresh_successor() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));
    for n in 1..=5 {
        study.preload_admin_of(&r0, 0, &format!("group-{n}"));
    }
    study.subscribe(
        std::slice::from_ref(&r0),
        "subscribe (5 values already cached)",
    );
    assert_eq!(
        study.ledger.live_outer_count(),
        1,
        "precondition: the warm set is ONE live outer sub"
    );
    assert_eq!(study.ledger.widest_outer(), 5);

    let step = study.admin_of(&r0, 0, "s", "group-6", "admin of a SIXTH group");
    study
        .ledger
        .report("B2. warm set of 5, then one more arriving live");

    assert_eq!(
        step,
        StepCount {
            opened: 1,
            replaced: 0,
            closed: 1
        },
        "the sixth value must use a fresh successor, with the predecessor \
         retained until exact local acceptance"
    );
    assert_eq!(study.ledger.live_outer_count(), 1);
    assert_eq!(study.ledger.widest_outer(), 6);
    assert_eq!(study.ledger.total.closed, 1);
    assert_eq!(study.ledger.total.replaced, 0);
}

// ---- C. growth before vs after EOSE ------------------------------------

/// C — does inner-set growth AFTER the inner subscription's EOSE behave
/// differently from growth before it? EOSE is the only real state boundary
/// in a zero-I/O harness: it closes acquisition evidence for that sub.
#[test]
fn c_growth_after_inner_eose_behaves_identically_to_growth_before() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));
    study.subscribe(std::slice::from_ref(&r0), "subscribe");

    let before = study.admin_of(&r0, 0, "s", "group-1", "PRE-EOSE  admin of group-1");
    let inner = study.inner_sub_id();
    study.eose(&r0, 0, &inner, "EOSE inner sub");
    let after = study.admin_of(&r0, 0, "s", "group-2", "POST-EOSE admin of group-2");

    study
        .ledger
        .report("C. one value before the inner EOSE, one after");

    // The two steps are no longer identical, and the difference is the
    // collapse rather than EOSE: the FIRST value opens the outer sub, the
    // second opens a fresh successor and its acceptance closes the old one.
    assert_eq!(
        before,
        StepCount {
            opened: 1,
            replaced: 0,
            closed: 0
        },
        "the first resolved value opens the outer sub"
    );
    assert_eq!(
        after,
        StepCount {
            opened: 1,
            replaced: 0,
            closed: 1
        },
        "EOSE on the inner sub must not change how a newly resolved derived \
         value reaches the wire: the second value uses the same accepted \
         successor transition as it would have pre-EOSE"
    );
    assert_eq!(
        study.ledger.live_outer_count(),
        1,
        "both values are served by ONE outer sub"
    );
    assert_eq!(study.ledger.widest_outer(), 2);
    assert_eq!(study.ledger.total.closed, 1);
    assert_eq!(study.ledger.total.replaced, 0);
}

// ---- D. a relay that streams but never EOSEs ---------------------------

/// D — a misbehaving relay that serves data and never EOSEs. Every resolved
/// value must still reach the wire: derived resolution is driven by ingested
/// rows, not by end-of-stored-events.
///
/// The load-bearing point survives the collapse and is now sharper: a fix
/// that bought the collapse by BATCHING ON EOSE would show up here as a
/// filter that never grows past nothing.
#[test]
fn d_never_eosing_relay_still_serves_every_value() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));
    study.subscribe(std::slice::from_ref(&r0), "subscribe");

    for n in 1..=4 {
        let group = format!("group-{n}");
        study.admin_of(
            &r0,
            0,
            "s",
            &group,
            &format!("admin of {group} (no EOSE ever)"),
        );
    }

    study
        .ledger
        .report("D. relay streams 4 values and never EOSEs");

    assert_eq!(
        study.ledger.live_outer_count(),
        1,
        "four values, one outer sub"
    );
    assert_eq!(
        study.ledger.widest_outer(),
        4,
        "a never-EOSEing relay must not stall derived resolution -- every value \
         must be in the live filter without any end-of-stored-events signal"
    );
    assert_eq!(study.ledger.total.closed, 3);
    assert_eq!(study.ledger.total.replaced, 0);
}

// ---- E. two relays, interleaved ----------------------------------------

/// E — the "slow second relay" case, expressed as the only thing a zero-I/O
/// engine can actually distinguish: values arriving interleaved across two
/// distinct relay sessions.
///
/// INVERTED, and the number to expect is PER RELAY rather than 1: coalescing
/// is partitioned by `(RelaySessionKey, ReadRouting)`
/// (`Router::compile`), so two pinned relays are two partitions and the
/// collapsed answer is ONE sub EACH -- two in total, each carrying all four
/// values. It previously asserted at least four (one per value), which is the
/// per-(value, relay) fan-out this file was written to measure.
#[test]
fn e_values_arriving_across_two_relays_collapse_per_relay_not_per_value() {
    let (r0, r1) = (relay(0), relay(1));
    let mut study = Study::new(&[r0.clone(), r1.clone()]);
    study.subscribe(&[r0.clone(), r1.clone()], "subscribe (2 pinned relays)");

    study.admin_of(&r0, 0, "s", "group-1", "relay0: group-1");
    study.admin_of(&r1, 1, "s", "group-2", "relay1: group-2 (the slow one)");
    study.admin_of(&r0, 0, "s", "group-3", "relay0: group-3");
    let inner = study.inner_sub_id();
    study.eose(&r0, 0, &inner, "relay0 EOSE (fast)");
    study.admin_of(&r1, 1, "s", "group-4", "relay1: group-4 (late)");
    study.eose(&r1, 1, &inner, "relay1 EOSE (slow)");

    study
        .ledger
        .report("E. two pinned relays, interleaved arrival, staggered EOSE");

    // Four distinct groups, two pinned relays. The fan-out WAS per (value,
    // relay) -- the multiplicative cost. It is now one sub per relay.
    assert_eq!(study.ledger.total.closed, 6);
    assert_eq!(study.ledger.total.replaced, 0);
    assert_eq!(
        study.ledger.live_outer_count(),
        2,
        "one outer sub per pinned relay -- coalescing is partitioned per relay \
         session, so two relays is two subs, not one and not one per value; got {:?}",
        study.ledger.live_outer()
    );
    for values in study.ledger.live_outer() {
        assert_eq!(
            values.len(),
            4,
            "each relay's single sub must ask for every resolved value: {values:?}"
        );
    }
}

// ---- F. scale ----------------------------------------------------------

/// F — scale, stated as a number rather than an adjective. 50 admin groups is
/// a realistic mid-size mosaico catalog.
///
/// INVERTED: this asserted 50 live outer subs against a real-world relay
/// ceiling of roughly 20. The catalog is now ONE subscription carrying 50
/// values, against a 500-value budget.
#[test]
fn f_fifty_values_are_one_outer_sub() {
    let r0 = relay(0);
    let mut study = Study::new(std::slice::from_ref(&r0));
    study.subscribe(std::slice::from_ref(&r0), "subscribe");

    for n in 1..=50 {
        study.admin_of(&r0, 0, "s", &format!("group-{n:02}"), "growth");
    }

    println!("\n=== F. 50 derived values ===");
    println!(
        "live outer subs:            {}",
        study.ledger.live_outer_count()
    );
    println!("cumulative REQs opened:     {}", study.ledger.total.opened);
    println!(
        "cumulative REQs replaced:   {}",
        study.ledger.total.replaced
    );
    println!("cumulative CLOSEs:          {}", study.ledger.total.closed);
    println!(
        "max #d values in any one REQ: {}",
        study.ledger.widest_outer()
    );

    assert_eq!(
        study.ledger.live_outer_count(),
        1,
        "50 catalog entries must be ONE subscription, not 50 against a relay \
         ceiling of ~20"
    );
    assert_eq!(
        study.ledger.widest_outer(),
        50,
        "that one subscription carries every value"
    );
    assert_eq!(study.ledger.total.closed, 49);
    assert_eq!(study.ledger.total.replaced, 0);
    assert_eq!(
        study.ledger.total.opened, 51,
        "one inner sub plus fifty fresh outer generations were offered"
    );
}

// ---- G. the decisive contrast: same shape, different slot ---------------

/// The canonical `$myFollows` shape, but pinned so it shares E's routing
/// context: `kinds:[1], authors := Derived(inner = kind:3 by me, project =
/// #p)`. Structurally identical to the query under study — a derived binding
/// whose set grows one element at a time — differing ONLY in which slot the
/// binding occupies.
fn posts_by_my_follows(relays: &[RelayUrl]) -> LiveQuery {
    let pinned: BTreeSet<RelayUrl> = relays.iter().cloned().collect();
    let inner = nmp_grammar::Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        ReadRouting::Explicit(pinned.clone().into_iter().collect()),
    )
    .expect("pinned inner demand with a nonempty relay set is constructible");
    let outer = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner,
            project: nmp_grammar::Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    };
    LiveQuery::single(
        nmp_grammar::Demand::new(outer, ReadRouting::Explicit(pinned.into_iter().collect()))
            .expect("pinned outer demand with a nonempty relay set is constructible"),
    )
}

/// G — THE contrast, now an EQUALITY. Grow a derived set one element at a
/// time, twice: once with the binding in `authors`, once in a tag slot. Same
/// engine, same growth, same relay, same pinned source.
///
/// This was the decisive asymmetry: `authors` collapsed to one current sub
/// while a tag slot opened one sub per value, because the registry
/// held an author rule and no tag rule, and because `Skeleton::of` erased
/// `authors` and nothing else. The asymmetry was always mechanical rather
/// than semantic -- a value list is a CHOICE in either slot -- and both
/// mechanisms are now slot-agnostic: one structural rule spanning every array
/// axis, and allocated tokens matched by structural signature.
///
/// So the test is no longer a contrast to be explained but a REGRESSION GUARD
/// on the equality. If the two slots ever diverge again, one of them has
/// grown a special case.
#[test]
fn g_a_derived_set_collapses_the_same_way_in_the_authors_slot_and_a_tag_slot() {
    let r0 = relay(0);

    // --- authors slot ---
    let mut core = new_core(FixtureRoutingFacts::new());
    connect(&mut core, 0, &r0);
    let me = Keys::generate();
    core.handle(EngineMsg::SetActivePubkey(Some(me.public_key())));
    let handles = BTreeMap::from([(
        public_session(&r0),
        RelayHandle {
            slot: 0,
            generation: 1,
        },
    )]);
    let mut authors_ledger = WireLedger::for_kinds(&[1]);
    let effects = core.handle_and_flush(EngineMsg::Subscribe(posts_by_my_follows(
        std::slice::from_ref(&r0),
    )));
    record_accepted_step(
        &mut core,
        &handles,
        &mut authors_ledger,
        "subscribe",
        effects,
    );

    let follows: Vec<Keys> = (0..5).map(|_| Keys::generate()).collect();
    for n in 1..=5 {
        let list: Vec<nostr::PublicKey> = follows[..n].iter().map(|k| k.public_key()).collect();
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&r0),
            event_frame("s", nmp_resolver_testkit::kind3(&me, &list, 100 + n as u64)),
        ));
        record_accepted_step(
            &mut core,
            &handles,
            &mut authors_ledger,
            &format!("follow #{n} (authors slot)"),
            effects,
        );
    }
    authors_ledger.report("G1. derived binding in the AUTHORS slot, 5 values");
    let authors_live_outer = authors_ledger.live_outer_count();
    let authors_widest = authors_ledger.widest_outer();

    // --- tag slot ---
    let mut study = Study::new(std::slice::from_ref(&r0));
    study.subscribe(std::slice::from_ref(&r0), "subscribe");
    for n in 1..=5 {
        study.admin_of(
            &r0,
            0,
            "s",
            &format!("group-{n}"),
            &format!("group #{n} (tag slot)"),
        );
    }
    study
        .ledger
        .report("G2. derived binding in a TAG slot, 5 values");

    println!(
        "\nCONTRAST  authors slot: {} live outer sub(s), widest carries {} values, \
         {} opened / {} replaced / {} closed",
        authors_live_outer,
        authors_widest,
        authors_ledger.total.opened,
        authors_ledger.total.replaced,
        authors_ledger.total.closed
    );
    println!(
        "CONTRAST  tag slot:     {} live outer sub(s), widest carries {} values, \
         {} opened / {} replaced / {} closed",
        study.ledger.live_outer_count(),
        study.ledger.widest_outer(),
        study.ledger.total.opened,
        study.ledger.total.replaced,
        study.ledger.total.closed
    );

    assert_eq!(
        authors_live_outer, 1,
        "five derived AUTHORS must collapse onto one wire sub"
    );
    assert_eq!(
        authors_widest, 5,
        "that one sub must carry all five authors — the union widened it"
    );
    assert_eq!(
        study.ledger.live_outer_count(),
        authors_live_outer,
        "five derived TAG values must collapse EXACTLY as five derived authors \
         do -- same sub count"
    );
    assert_eq!(
        study.ledger.widest_outer(),
        authors_widest,
        "...and the same value count in the one filter"
    );
    assert_eq!(
        study.ledger.total.closed, authors_ledger.total.closed,
        "...and both slots retire the same number of accepted predecessors"
    );
    assert_eq!(authors_ledger.total.replaced, 0);
    assert_eq!(study.ledger.total.replaced, 0);
}

// ---- I. what a regrouping REQ actually costs ---------------------------

/// I — the hidden cost of collapsing subscriptions, and the bound on it.
///
/// No wire filter this router builds ever carries `since` (verified: nothing
/// in `nmp-router` sets it). So a fresh successor carrying a wider filter makes
/// the relay re-serve its whole stored set for the NEW filter — including
/// every event already delivered under the old one. That is the real cost of
/// regrouping, and it is invisible to a ledger that counts wire messages.
///
/// This measures whether that re-serve costs ROWS as well as bandwidth. It
/// must not: canonical dedup should absorb a re-served event entirely, so
/// collapsing subscriptions is a bandwidth question only — which is what
/// makes debounce an optimization rather than a correctness requirement.
#[test]
fn i_re_served_events_after_a_successor_cost_bandwidth_but_never_rows() {
    let r0 = relay(0);
    let mut core = new_core(FixtureRoutingFacts::new());
    connect(&mut core, 0, &r0);
    let me = Keys::generate();
    core.handle(EngineMsg::SetActivePubkey(Some(me.public_key())));

    let handles = BTreeMap::from([(
        public_session(&r0),
        RelayHandle {
            slot: 0,
            generation: 1,
        },
    )]);
    let subscribed = core.handle_and_flush(EngineMsg::Subscribe(posts_by_my_follows(
        std::slice::from_ref(&r0),
    )));
    let _ = accept_requests(&mut core, &handles, &subscribed);
    let mut delivered_rows = 0usize;

    let follows: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
    let mut posts = Vec::new();
    for (n, author) in follows.iter().enumerate() {
        // Follow one more author — this opens a wider successor, whose exact
        // local acceptance retires the predecessor.
        let list: Vec<nostr::PublicKey> = follows[..=n].iter().map(|k| k.public_key()).collect();
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&r0),
            event_frame("s", nmp_resolver_testkit::kind3(&me, &list, 200 + n as u64)),
        ));
        delivered_rows += effect_row_delta_count(&effects);
        let accepted = accept_requests(&mut core, &handles, &effects);
        delivered_rows += effect_row_delta_count(&accepted);
        let post = nmp_resolver_testkit::kind1(author, "post", 300 + n as u64);
        posts.push(post.clone());
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&r0),
            event_frame("s", post),
        ));
        delivered_rows += effect_row_delta_count(&effects);
    }

    let rows_before = delivered_rows;

    // The relay re-serves everything the widened filter now matches — the
    // full stored set, every previously delivered post included.
    for post in &posts {
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&r0),
            event_frame("s", post.clone()),
        ));
        delivered_rows += effect_row_delta_count(&effects);
    }

    let rows_after = delivered_rows;

    println!("\n=== I. cost of a re-serve after an accepted successor ===");
    println!("distinct posts:              {}", posts.len());
    println!("row deltas before re-serve:  {rows_before}");
    println!("row deltas after re-serve:   {rows_after}");
    println!(
        "rows added by re-serving every stored event: {}",
        rows_after - rows_before
    );

    assert_eq!(
        rows_after, rows_before,
        "a re-served event must produce ZERO new row deltas — collapsing subscriptions \
         costs relay bandwidth, never duplicated rows"
    );
}
