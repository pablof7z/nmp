//! Fresh request replacement transition ownership (#774).

use nmp_grammar::RelaySessionKey;
use std::collections::BTreeSet;

use nmp_store::RedbStore;

use super::query::PlanDeltaMode;
use super::*;

struct Fixture {
    core: EngineCore,
    relay: RelayUrl,
    handle: TransportRelayHandle,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let relay = RelayUrl::parse(&format!("wss://{name}.example")).unwrap();
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let handle = TransportRelayHandle {
            slot: 93,
            generation: 1,
        };
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
        core.white_box("slot_to_relay.insert", |s| {
            s.slot_to_relay
                .insert(handle.slot, (handle, session.clone()))
        });
        core.white_box("connected_relays.insert", |s| {
            s.connected_relays.insert(session.clone())
        });
        core.white_box("ever_connected_relays.insert", |s| {
            s.ever_connected_relays.insert(session.clone())
        });
        Self {
            core,
            relay,
            handle,
        }
    }

    fn atom(&self, author_byte: u8) -> ContextualAtom {
        ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(BTreeSet::from([format!("{author_byte:02x}").repeat(32)])),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![self.relay.clone()]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        }
    }

    /// One recompile, in the order `CoreState::recompile` performs it: install
    /// `demand` as attribution's current logical demand, then compile the
    /// router from the same set. Each caller used to hand-write the
    /// attribution half — `observe_atom` for whatever arrived and
    /// `release_atom` for whatever left, twenty sites across five tests — so
    /// the fixture's "recompile" and the reducer's could drift apart with
    /// nothing to catch it (#1850).
    fn compile(&mut self, demand: BTreeSet<ContextualAtom>) -> Vec<Effect> {
        let outcome = self.core.white_box("recompile", |s| {
            s.attribution.set_active_demand(demand.iter());
            s.router
                .compile(&demand, &s.routing_facts, s.compile_budget())
        });
        let mut effects = Vec::new();
        self.core.white_box("apply_request_metadata_updates", |s| {
            s.apply_request_metadata_updates(&outcome.request_metadata_updates, &mut effects)
        });
        self.core.white_box("apply_router_plan_delta", |s| {
            s.apply_router_plan_delta(
                &outcome.replacements,
                outcome.wire,
                PlanDeltaMode::Full,
                &mut effects,
            )
        });
        effects
    }

    fn accept(&mut self, effects: &[Effect]) -> SubId {
        let (_, sub_id, _, attempt_id) = only_request(effects);
        self.core
            .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                attempt_id,
                handle: self.handle,
            });
        sub_id
    }

}

fn only_request(effects: &[Effect]) -> (RelaySessionKey, SubId, ConcreteFilter, RequestAttemptId) {
    effects
        .iter()
        .find_map(|effect| {
            let Effect::Wire(delta) = effect else {
                return None;
            };
            delta.ops.iter().find_map(|(session, ops)| {
                ops.iter().find_map(|op| {
                    let WireOp::Req(sub_id, filter) = op else {
                        return None;
                    };
                    Some((
                        session.clone(),
                        sub_id.clone(),
                        filter.clone(),
                        delta.attempt_id(session, sub_id, filter),
                    ))
                })
            })
        })
        .expect("one request effect")
}

#[test]
fn accepted_byte_changed_replacements_retain_only_one_current_request() {
    let mut fixture = Fixture::new("bounded-accepted-replacements");
    let opened = fixture.compile(BTreeSet::from([fixture.atom(10)]));
    let mut current_sub = fixture.accept(&opened);

    for author in 11..=42 {
        let next = fixture.atom(author);
        let replacement = fixture.compile(BTreeSet::from([next]));
        let (_, next_sub, _, next_attempt) = only_request(&replacement);
        assert_ne!(current_sub, next_sub);
        assert_eq!(
            fixture
                .core
                .bench_ownership_census()
                .request_replacement_jobs,
            1
        );
        fixture
            .core
            .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                attempt_id: next_attempt,
                handle: fixture.handle,
            });
        let census = fixture.core.bench_ownership_census();
        assert_eq!(census.request_replacement_jobs, 0);
        assert_eq!(census.live_wire_owners, 1);
        assert_eq!(census.active_execution_owners, 1);
        current_sub = next_sub;
    }

    fixture.compile(BTreeSet::new());
    assert_eq!(
        fixture.core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
