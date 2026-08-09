Feature: Relay work waits briefly for compatible pending demand
  An app may own thousands of independent observations; it never has to batch,
  shard, or pre-aggregate them for NMP. Each observation keeps its own local
  projection, evidence, and cancellation. Only unsent relay work waits: the
  first uncovered observation opens a 10ms cohort, NMP groups compatible relay
  demand behind that boundary, and the resulting REQs become immutable once
  admitted.

  Rule: A pending cohort delays wire work, never cache delivery

    # nmp:id=ROUTING-PENDING-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::cache_seed_is_immediate_while_wire_execution_waits_for_admission_flush
    # nmp:evidence=rust:nmp::unbounded_profile_observations_group_without_losing_independent_owners
    # nmp:evidence=rust:nmp::window_is_anchored_to_first_arrival_and_rearms_for_the_next_cohort
    # nmp:evidence=rust:nmp::runtime_admission_deadline_groups_a_rapid_query_burst
    # nmp:falsifier=Collapse app-owned observations into one cancellation, delay an observation's local frame, slide the deadline on each arrival, or compile each compatible observation separately; the independent identities, local seeds, timing, or grouped wire proof fails.
    Scenario: Independent avatar observations cause one grouped relay request
      Given several independently cancellable avatar observations need kind:0 profiles from the same relay
      And each profile observation is unbounded because its demand has no result limit
      When those observations open inside one 10ms admission cohort
      Then every observation receives its own local projection and evidence immediately
      And NMP alone groups their compatible relay demand into one unbounded request
      And later arrivals cannot extend the cohort's first-arrival deadline

  Rule: Sent requests are immutable admission facts

    # nmp:id=ROUTING-PENDING-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::later_uncovered_demand_opens_a_second_req_without_replacing_the_running_one
    # nmp:evidence=rust:nmp-router::one_pending_cohort_coalesces_but_a_later_cohort_never_rewrites_it
    # nmp:falsifier=Admit uncovered demand by widening or replacing an already-sent request; the wire delta contains a close or loses the incumbent request.
    Scenario: A second admission wave opens another request
      Given an earlier admission wave already sent a running relay request
      When a second admission wave asks for compatible but uncovered demand
      Then the earlier request stays byte-for-byte unchanged
      And the second wave opens an additional request
      And no close is sent for the earlier request

    # nmp:id=ROUTING-PENDING-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::duplicate_running_demand_attaches_without_compile_or_sibling_projection
    # nmp:evidence=rust:nmp::pending_handoff_resolves_the_current_exact_owner_set
    # nmp:evidence=rust:nmp::outstanding_request_terminals_follow_current_exact_owners_after_attachment_churn
    # nmp:evidence=rust:nmp-router::exact_running_coverage_makes_repeated_admission_a_noop
    # nmp:falsifier=Treat exact active coverage as pending or freeze execution ownership at send time; the duplicate query recompiles, emits another REQ, reports settled work as outstanding, or delivers a later terminal to a detached owner.
    Scenario Outline: Exact demand attaches to the truthful incumbent request phase
      Given an earlier admission wave has a request covering the exact logical demand that is <phase>
      When another independent observation asks for that demand
      Then it receives its own cached projection immediately
      And NMP emits no relay request and performs no router compile
      And its acquisition evidence reports <current phase>
      And any still-outstanding settlement or close reports only to the exact owners attached when that terminal arrives
      And an already-settled request does not replay historical request or settlement facts

      Examples:
        | phase                              | current phase          |
        | accepted and awaiting a terminal   | stored events streaming |
        | already settled and still live     | stored events finished  |

  Rule: Observation lifecycle work is proportional to the observation changed

    # nmp:id=ROUTING-PENDING-004
    # nmp:status=built
    # nmp:evidence=rust:nmp::a_large_open_and_close_burst_never_reprojects_sibling_rows
    # nmp:evidence=rust:nmp::history_open_waits_for_the_same_flush_without_refreshing_an_ordinary_sibling
    # nmp:falsifier=Open or close one observation by rereading another observation's canonical rows; the exact projection counters exceed the observations whose own view was opened.
    Scenario: Opening and closing many observations never reprojects siblings
      Given many observations already have independent cached projections
      When more ordinary or windowed observations open or close
      Then only each newly opened observation reads its own canonical rows
      And plan changes refresh acquisition evidence only for affected demand
      And closing observations does not reread surviving rows

    # nmp:id=ROUTING-PENDING-005
    # nmp:status=built
    # nmp:evidence=rust:nmp::ten_thousand_shared_bounded_owners_withdraw_in_owner_plus_one_close_work
    # nmp:evidence=rust:nmp-router::withdrawal_keeps_a_shared_immutable_req_until_its_last_key_leaves
    # nmp:evidence=rust:nmp-router::ten_thousand_shared_keys_do_only_delta_edges_plus_one_physical_close
    # nmp:falsifier=Reconstruct sibling demand, couple independent cancellations, shrink physical coverage, or close before the last exact owner leaves; the 10k identities, structural work counters, local reattach proof, or exact wire-close count fails.
    Scenario: Independent observations withdraw by exact owner delta
      Given ten thousand independently cancellable observations share bounded demand covered by immutable relay requests
      When each observation withdraws through its own cancellation
      Then each non-final withdrawal touches only its departing exact ownership edge
      And it leaves sibling projections and evidence unchanged
      And it reads no sibling projection or coverage and emits no wire or diagnostics frame
      And the final owner emits exactly one close for each physical request
      And detached exact demand can reattach to a still-running covering request without a new REQ

    # nmp:id=ROUTING-PENDING-006
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::withdrawing_the_final_routeless_outbox_owner_retracts_its_diagnostic
    # nmp:evidence=rust:nmp::withdrawing_the_final_routeless_observation_emits_its_diagnostic_retraction
    # nmp:falsifier=Gate diagnostic refresh only on a physical request close; final withdrawal of routeless outbox demand leaves its author falsely reported as uncovered or emits no retraction frame.
    Scenario: Final routeless ownership retracts its diagnostic without wire work
      Given one live observation owns outbox demand with no candidate relay
      And diagnostics report its author as uncovered
      When that observation withdraws its final exact ownership
      Then no relay request or close is emitted
      And the uncovered-author diagnostic is removed in the same reducer call

    # nmp:id=ROUTING-PENDING-007
    # nmp:status=built
    # nmp:evidence=rust:nmp::ten_thousand_distinct_pending_cancellations_never_rebuild_surviving_demand
    # nmp:falsifier=Reconstruct the pending cohort after each exact pre-admission cancellation; the structural counter reports surviving pending atoms rebuilt even though no relay plan changed.
    Scenario: Pre-admission cancellation removes only its pending ownership
      Given distinct compatible observations are pending and no request has been sent
      When one observation cancels before the admission boundary
      Then only its exact pending atom is removed
      And surviving pending demand is neither inspected nor reconstructed
      And no store read, router compile, diagnostic frame, or wire operation occurs

    # nmp:id=ROUTING-PENDING-008
    # nmp:status=built
    # nmp:evidence=rust:nmp::reattaching_a_covered_atom_keeps_its_shared_immutable_request_active
    # nmp:evidence=rust:nmp::delayed_accepted_handoff_cannot_resurrect_a_fully_withdrawn_request
    # nmp:falsifier=Leave reattached exact demand inactive under its covering request, or accept a delayed transport handoff after final cancellation; closing a sibling prematurely closes the immutable request or resurrects local execution ownership.
    Scenario: Reattached demand keeps its already-sent request alive
      Given two independent observations share one already-sent immutable request
      And one observation withdraws and later reopens while the sibling still owns that request
      When the sibling withdraws
      Then the already-sent request remains byte-for-byte unchanged and no close is sent
      And the reopened observation remains independently cancellable
      And its final withdrawal emits exactly one close
      And a delayed acceptance for the retired request cannot restore observation or wire ownership

    # nmp:id=ROUTING-PENDING-009
    # nmp:status=built
    # nmp:evidence=rust:nmp::a_later_admission_cohort_never_visits_ten_thousand_incumbents
    # nmp:evidence=rust:nmp-router::later_cohort_never_rebuilds_or_visits_ten_thousand_incumbent_active_entries
    # nmp:falsifier=Rebuild incumbent demand, requests, pending ownership, or refusal diagnostics when admitting one later cohort; structural counters visit standing entries or an earlier request or refusal changes.
    Scenario: A later cohort touches no incumbent relay ownership
      Given ten thousand admitted relay requests and an earlier refusal remain active
      When one later uncovered observation reaches its admission boundary
      Then NMP compiles only that new cohort
      And no incumbent demand, request, pending owner, or refusal diagnostic is visited
      And every earlier request and refusal remains byte-for-byte unchanged
      And acquisition evidence is refreshed only for the newly covered observation

    # nmp:id=ROUTING-PENDING-010
    # nmp:status=built
    # nmp:evidence=rust:nmp::local_owner_detach_prunes_the_current_attribution_generation_before_eose
    # nmp:falsifier=Keep a departed observation's claim in the current request generation after its exact local owner ends; partial churn retains stale per-key ownership until an unrelated owner or the physical request closes.
    Scenario: Departed attribution shapes do not wait for unrelated owners
      Given two independent observations contribute different current pre-EOSE claim shapes
      When one observation withdraws and its exact current request claim ownership ends
      Then its attribution shape is released immediately
      And the unrelated observation and its shape remain active

    # nmp:id=ROUTING-PENDING-013
    # nmp:status=built
    # nmp:evidence=rust:nmp::aliased_current_claim_stays_until_its_last_owner_and_can_reattach_before_eose
    # nmp:falsifier=Remove a claim on the first aliased owner departure, retain it after its last exact owner leaves, or fail to restore it on pre-EOSE reattachment; a live owner loses coverage or a departed owner earns stale coverage.
    Scenario: Current request claims follow exact local ownership before EOSE
      Given one immutable request has a claim shared by two exact local owners
      When one owner withdraws before end of stored events
      Then the remaining alias keeps the claim in the current generation
      When the last exact owner withdraws
      Then the current generation drops that claim without rewriting wire bytes
      And a late EOSE cannot persist the departed claim
      And reattachment before EOSE restores the claim for that current generation

    # nmp:id=ROUTING-PENDING-011
    # nmp:status=built
    # nmp:evidence=rust:nmp::disjoint_routing_evidence_owners_remain_exact_in_both_close_orders
    # nmp:evidence=rust:nmp::a_covered_owner_can_add_the_first_rejected_fact_without_rewriting_wire
    # nmp:falsifier=Store one representative atom for owners that share a selection but carry different routing facts; one fact disappears on open or closing either owner erases the survivor, while an already-sent request may be rewritten.
    Scenario: Shared selection keeps each owner's routing facts independently cancellable
      Given independent observations share one exact selection and contribute different routing evidence
      When either observation withdraws first
      Then the effective routing evidence is exactly the union of the owners still active
      And rejected routing facts retain only their live exact ownership
      And pending or limited work keeps admission armed for that one selection
      And an already-sent request remains byte-for-byte unchanged
      And the final owner releases all routing-evidence and rejected-evidence ownership

    # nmp:id=ROUTING-PENDING-014
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::partially_served_outbox_demand_owns_its_exact_shortfall_with_a_live_request
    # nmp:evidence=rust:nmp-router::same_author_distinct_shortfalls_reveal_the_exact_survivor_in_both_orders
    # nmp:evidence=rust:nmp-router::simultaneous_shortfall_reduction_is_semantic_not_demand_key_order
    # nmp:falsifier=Infer uncovered-author ownership from physical request presence or a collapsed author map; a partially served k=2 demand loses its shortfall, withdrawing one same-author demand leaves the sibling's wrong reason, or key order changes the public fact.
    Scenario: Every logical outbox demand owns its exact shortfall contribution
      Given one author has independent logical outbox demands with different routing outcomes
      And a partially served k=2 demand may own one immutable request and one remaining deficit
      When either exact demand withdraws first
      Then the survivor's requested count, achieved count, and reason equal a fresh compile of that survivor
      And the public author fact reduces simultaneous contributions by greatest deficit and stable reason priority
      And DemandKey or input ordering cannot change that public fact

    # nmp:id=ROUTING-PENDING-015
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::second_projected_hint_adds_only_the_missing_session_and_heals_shortfall
    # nmp:evidence=rust:nmp::second_outbox_hint_opens_only_the_missing_relay_for_both_owner_close_orders
    # nmp:evidence=rust:nmp::preflush_hint_owner_churn_combines_pending_and_incumbent_assignment_truth
    # nmp:falsifier=Treat one incumbent request edge as complete k-of-n admission or calculate a cohort's shortfall without incumbent assignment; a new unique hint opens nothing, rewrites the first REQ, or leaves a stale deficit after pre-flush owner churn.
    Scenario: A later projected hint heals only the missing assignment
      Given one owner supplied one relay hint for a k=2 demand and its immutable request is active
      When another exact owner contributes a second unique relay hint
      Then NMP opens exactly one new request to the missing relay
      And the incumbent request remains byte-for-byte unchanged
      And combined incumbent plus cohort assignment retracts the shortfall
      And withdrawing either owner before or after the pending flush keeps only the live evidence union
      And a duplicate hint performs no compile or wire work

    # nmp:id=ROUTING-PENDING-012
    # nmp:status=built
    # nmp:evidence=rust:nmp::incompatible_requests_visit_only_their_exact_execution_targets
    # nmp:evidence=rust:nmp::unbounded_profile_observations_group_without_losing_independent_owners
    # nmp:evidence=rust:nmp::cache_only_siblings_are_not_execution_targets_of_a_live_request
    # nmp:evidence=rust:nmp::a_shared_request_targets_every_wire_active_owner_and_no_cache_only_sibling
    # nmp:evidence=rust:nmp::window_distinct_requests_target_only_their_exact_demand_owners_on_send_and_replay
    # nmp:evidence=rust:nmp::nested_same_demand_boundaries_target_only_wire_participating_scopes
    # nmp:evidence=rust:nmp::reactive_nested_same_demand_replaces_only_the_live_scope_target_revision
    # nmp:evidence=rust:nmp::nip77_candidate_and_fallback_target_only_the_current_wire_participating_scope
    # nmp:evidence=rust:nmp-resolver::snapshot_scopes_are_per_traversal_occurrence_even_when_a_filter_node_is_shared
    # nmp:evidence=rust:nmp::changed_filter_revisions_replace_stale_request_targets_before_send
    # nmp:falsifier=Discover each request's observation evidence by scanning every observation, collapse distinct limits/windows or nested acquisition scopes into one target, target a CacheOnly or coverage-satisfied path, or retain an earlier filter revision after reactive resolution changes; incompatible admissions perform quadratic owner work or deliver a relay lifecycle fact to a sibling, non-wire scope, or stale revision.
    Scenario: A sent request reports only to the observations it absorbed
      Given many independent observations resolve to current concrete filters
      And some same-filter observations are cache-only while others own relay work
      And same-selection observations with different limits or windows remain distinct relay demand
      And nested Demand boundaries may resolve the same exact relay demand while only one boundary owns wire work
      When NMP sends separate incompatible requests or one compatible grouped request
      Then each request visits every wire-active absorbed observation exactly once
      And no cache-only or unrelated sibling receives a relay-request fact
      And each window-distinct request reports only to its exact logical owner on send and replay
      And each nested request reports only to its wire-participating structural occurrence
      And a NIP-77 candidate, reconciliation, refusal, and fallback retain that same occurrence distinction
      And either close order and a later live reopen preserve that distinction
      And a changed filter revision replaces the earlier target before active relay work reattaches
      And final cancellation releases every execution-evidence owner

    # nmp:id=ROUTING-PENDING-016
    # nmp:status=built
    # nmp:evidence=rust:nmp::eose_refreshes_live_evidence_without_event_index_query
    # nmp:evidence=rust:nmp::coalesced_eose_refreshes_its_two_current_owners_once_each
    # nmp:evidence=rust:nmp::limited_eose_refreshes_only_its_ordinary_and_history_request_phase
    # nmp:evidence=rust:nmp::neg_completion_refreshes_only_its_exact_current_owners
    # nmp:falsifier=Refresh every observation or history when one EOSE or NEG completion changes coverage or request phase; sibling candidates, coverage reads, or frames grow with unrelated owners, or a bounded completion never reports finished stored events.
    Scenario: Request terminals refresh only exact current owners
      Given many unrelated ordinary observations and histories remain active
      When one ordinary EOSE or correlated NEG completion becomes trustworthy
      Then NMP visits only handles attached to its exact coverage keys and logical demands
      And one handle affected by both dimensions is refreshed only once
      And a bounded or poisoned EOSE records no false coverage while its current owner still reports finished stored events
      And a limit:0 NIP-77 barrier remains nonterminal
      And no sibling coverage read, evidence frame, or eager diagnostics snapshot is produced

    # nmp:id=ROUTING-PENDING-017
    # nmp:status=built
    # nmp:evidence=rust:nmp::repeated_local_refusals_keep_one_goal_increase_backoff_and_become_requesting_only_on_accept
    # nmp:evidence=rust:nmp::dynamic_full_recompile_publishes_awaiting_request_before_wire_dispatch
    # nmp:evidence=rust:nmp::empty_neg_completion_projects_finished_status_through_the_plan_request
    # nmp:evidence=rust:nmp::refused_neg_open_publishes_awaiting_before_its_fallback_request
    # nmp:evidence=rust:nmp::missing_id_backfill_publishes_awaiting_before_its_request
    # nmp:falsifier=Report a planned or locally refused send as Requesting or RelayRefused, emit wire before its AwaitingRequest evidence, retain more than one retry goal, or leave the goal alive after withdrawal.
    Scenario: Local placement stays awaiting until exact transport acceptance
      Given a connected source has one planned local request that is not yet accepted
      Then its source status is AwaitingRequest before wire dispatch
      When local transport refuses that placement
      Then execution records RequestDeferred and never RelayRefused or Requesting
      And exactly one engine-owned retry and deadline remain
      When the exact retry handoff is accepted
      Then the source status becomes Requesting
      And candidate, reconciliation, and repair role ids report through their plan source
      And withdrawal cancels the attempt or retry ownership
