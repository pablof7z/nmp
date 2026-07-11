Feature: Enough relays to be safe, never a flood
  @ledger-4 @wip
  Scenario Outline: Every author is read from at least two relays, under a cap
    # Genuine gap, scoped out of this foundation pass rather than faked
    # green (approach doc Appendix item 5): proving this for real against
    # the REAL engine means instantiating <authors> real in-process relays
    # per row (disjoint per-author write-relay pairs) and running the full
    # solver under real sockets -- a heavier lift than this batch's budget.
    # `nmp-router/tests/contract.rs`'s existing coverage/cap tests already
    # prove the solver headlessly; this scenario is the readable wrapper
    # still to be written.
    Given I am logged in as an account that follows <authors> people
    And every followed author's relay list is known
    When I open a feed of my follows' notes
    Then each followed author is served by at least 2 relays
    And no more than <cap> relays are contacted in total

    Examples:
      | authors | cap |
      | 5       | 10  |
      | 50      | 15  |
