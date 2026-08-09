Feature: Relay work waits briefly for compatible pending demand
  An app may own thousands of independent observations; it never has to batch,
  shard, or pre-aggregate them for NMP. Each observation keeps its own local
  projection, evidence, and cancellation. Only unsent relay work waits: the
  first uncovered observation opens a 30ms cohort, NMP groups compatible relay
  demand behind that boundary, and the resulting REQs become immutable once
  admitted.

  Rule: A pending cohort delays wire work, never cache delivery

    # nmp:id=ROUTING-PENDING-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::cache_seed_is_immediate_while_wire_execution_waits_for_admission_flush
    # nmp:evidence=rust:nmp::compatible_pending_observations_compile_once_into_one_relay_request
    # nmp:evidence=rust:nmp::window_is_anchored_to_first_arrival_and_rearms_for_the_next_cohort
    # nmp:evidence=rust:nmp::runtime_admission_deadline_groups_a_rapid_query_burst
    # nmp:falsifier=Collapse app-owned observations into one cancellation, delay an observation's local frame, slide the deadline on each arrival, or compile each compatible observation separately; the independent identities, local seeds, timing, or grouped wire proof fails.
    Scenario: Independent avatar observations cause one grouped relay request
      Given several independently cancellable avatar observations need profiles from the same relay
      When those observations open inside one 30ms admission cohort
      Then every observation receives its own local projection and evidence immediately
      And NMP alone groups their compatible relay demand into one request
      And later arrivals cannot extend the cohort's first-arrival deadline

  Rule: Sent requests are immutable admission facts

    # nmp:id=ROUTING-PENDING-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::later_uncovered_demand_opens_a_second_req_without_replacing_the_running_one
    # nmp:evidence=rust:nmp-router::one_pending_cohort_coalesces_but_a_later_cohort_never_rewrites_it
    # nmp:falsifier=Admit uncovered demand by widening or replacing an already-sent request; the wire delta contains a close or loses the incumbent request.
    Scenario: A later uncovered query opens another request
      Given an earlier admission cohort already has a running relay request
      When a later cohort asks for compatible but uncovered demand
      Then the earlier request stays byte-for-byte unchanged
      And the later cohort opens an additional request
      And no close is sent for the earlier request

    # nmp:id=ROUTING-PENDING-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::duplicate_running_demand_attaches_without_compile_or_sibling_projection
    # nmp:evidence=rust:nmp-router::exact_running_coverage_makes_repeated_admission_a_noop
    # nmp:falsifier=Treat exact active coverage as pending; the duplicate query recompiles, reads a sibling projection, or emits another REQ.
    Scenario: A query already covered by a running request only attaches locally
      Given a running request already covers the exact logical demand
      When another observation asks for that demand
      Then it receives its own cached projection immediately
      And NMP emits no relay request and performs no router compile

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
