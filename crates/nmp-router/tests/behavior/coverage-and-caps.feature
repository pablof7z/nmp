Feature: Coverage shortfall stays honest under one whole-demand relay ceiling
  A coverage objective is an objective, not a promise that impossible inputs
  can satisfy. The router applies one ceiling to the complete demand and says
  which otherwise-routable authors were excluded by that ceiling.

  Rule: Impossible coverage is reported rather than silently truncated
    # nmp:id=ROUTING-COVERAGEANDCAPS-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::coverage_respects_whole_demand_cap
    # nmp:falsifier=changing cap-exhausted shortfall to no-candidates makes the owner test fail
    @ledger-4
    Scenario: An impossible coverage objective reports shortfall under the cap
      Given ten authors have disjoint candidate relays
      And the whole-demand relay ceiling is 6
      When the router compiles the complete demand
      Then the plan contains no more than 6 relay sessions
      And every excluded routable author reports cap exhaustion
