Feature: Absence is knowledge; ignorance is not
  For every author a write depends on, the engine holds exactly one of three
  answers: it knows that author's relay list, it knows that author has none, or
  it has not finished looking. The last two collapse to the same empty set of
  relays and must never be confused. A settled absence lets a write finish
  routing; an unfinished look keeps it parked.

  Background:
    Given only 2 indexer relays are configured
    And I am logged in as my own account
    And my relay list names "wss://mine.example" as my write relay

  @designed
  Scenario: A relay list that declares no relays at all is knowledge
    Given Bob's relay list is ingested and names no relays at all
    When I publish a note mentioning Bob
    Then the note is published to "wss://mine.example"
    And no relay is contacted on Bob's behalf
    And routing for the note is complete
    And the note is never parked waiting on Bob

  @designed
  Scenario: An author never looked up to completion keeps the write parked
    Given no relay list for Carol has ever been ingested
    And the indexers have not confirmed end of stored events for Carol's relay list
    When I publish a note mentioning Carol
    Then the note is published to "wss://mine.example"
    And routing for the note is not complete
    And the note stays parked awaiting Carol's relay list
    And the note is not reported as failed

  @designed
  Scenario: A settled absence retires the write -- the three-mention case
    # Pablo: "an outbox can end too; for example, if the user is p-tagging 3
    # users and only one of them has a 10002 and we know the other two don't
    # have one [...] then the outbox item is consumed."
    Given Bob's relay list names "wss://bob-relay.example" as his read relay
    And no relay list for Carol or Dave exists
    And the indexers have confirmed end of stored events for Carol's relay list
    And the indexers have confirmed end of stored events for Dave's relay list
    When I publish a note mentioning Bob, Carol, and Dave
    Then the note is published to "wss://mine.example" and "wss://bob-relay.example"
    And routing for the note is complete
    And nothing is left parked on Carol or Dave

  @designed
  Scenario Outline: The same empty relay set means two different things
    Given no relay list for Carol has ever been ingested
    And the indexers have <lookup> confirmed end of stored events for Carol's relay list
    When I publish a note mentioning Carol
    Then no relays are known for Carol either way
    And routing for the note is <outcome>
    And the note is never reported as failed

    Examples:
      | lookup  | outcome      |
      | already | complete     |
      | not yet | not complete |

  @designed
  Scenario: A cold start never mistakes ignorance for absence
    Given the engine has just started and no relay list has been ingested yet
    And Bob's relay list names "wss://bob-relay.example" as his read relay
    And the indexers have not answered for Bob's relay list yet
    When I publish a note mentioning Bob
    Then routing for the note is not complete
    And no relay outside "wss://mine.example" and the indexers was contacted
    When the indexers deliver Bob's relay list and confirm end of stored events
    Then the note is published to "wss://bob-relay.example"
    And routing for the note is complete
