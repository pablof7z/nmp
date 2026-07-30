Feature: Enough relays to be safe, never a flood
  # nmp:id=ROUTING-COVERAGEANDCAPS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-router::feasible_two_source_author_coverage_stays_under_the_whole_demand_cap
  # nmp:falsifier=changing the product coverage request from k=2 to k=1 makes the exact owner proof fail
  @ledger-4
  Scenario Outline: Every author is read from at least two relays, under a cap
    Given I am logged in as an account that follows <authors> people
    And every followed author's relay list is known
    When I open a feed of my follows' notes
    Then each followed author is planned on at least 2 relay sessions
    And no more than <cap> relay sessions are planned in total

    Examples:
      | authors | cap |
      | 5       | 10  |
      | 50      | 15  |
