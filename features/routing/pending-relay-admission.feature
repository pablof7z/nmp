Feature: Relay work waits briefly for compatible pending demand
  An app can discover many live queries in one render pass. Their cached rows
  are local facts and return immediately. Only unsent relay work waits: the
  first uncovered query opens a 30ms cohort, compatible queries joining that
  cohort are routed and coalesced together, and the resulting REQs become
  immutable once admitted.

  Rule: A pending cohort delays wire work, never cache delivery

    # nmp:id=ROUTING-PENDING-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::cache_seed_is_immediate_while_wire_execution_waits_for_admission_flush
    # nmp:evidence=rust:nmp::compatible_pending_observations_compile_once_into_one_relay_request
    # nmp:evidence=rust:nmp::window_is_anchored_to_first_arrival_and_rearms_for_the_next_cohort
    # nmp:evidence=rust:nmp::runtime_admission_deadline_groups_a_rapid_query_burst
    # nmp:falsifier=Delay the opening cache frame, slide the deadline on each arrival, or compile each compatible query separately; one of the three timing and grouping proofs fails.
    Scenario: Several avatars entering together cause one grouped relay request
      Given several avatars need replaceable profiles from the same relay
      When their live queries open inside one 30ms admission cohort
      Then every avatar receives its current cached profile immediately
      And the cohort produces one relay request carrying every pending author
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
