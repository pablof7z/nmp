Feature: Enough relays to be safe, never a flood
  The router seeks two sources per author when that objective is feasible,
  while one whole-demand cap bounds the complete plan.

  Rule: Feasible shared coverage retains two sources per author
    # nmp:id=ROUTING-COVERAGEANDCAPS-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::feasible_two_source_author_coverage_stays_under_the_whole_demand_cap
    # nmp:falsifier=Reduce Router's author-coverage target from two relays to one; every author's second exact relay-session witness disappears.
    @ledger-4
    Scenario Outline: Every author is read from at least two relays, under a cap
      Given <authors> authors each name the same 2 outbound relays
      And the whole-demand relay ceiling is <cap>
      When their note queries are compiled into one relay plan
      Then every author is present on both relay sessions
      And no more than <cap> relays are contacted in total

      Examples:
        | authors | cap |
        | 5       | 10  |
        | 50      | 15  |
