Feature: Enough relays to be safe, never a flood
  @ledger-4
  Scenario Outline: Every author is read from at least two relays, under a cap
    Given I am logged in as an account that follows <authors> people
    And every followed author's relay list is known
    And all followed authors share the same two valid read relays
    When I open a feed of my follows' notes
    Then each followed author is planned on at least 2 relay sessions
    And no more than <cap> relay sessions are planned in total

    Examples:
      | authors | cap |
      | 5       | 10  |
      | 50      | 15  |
