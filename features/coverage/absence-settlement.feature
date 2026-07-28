Feature: End of stored events is what settles an absence
  Pablo's ruling, verbatim: 'and "do we have a 10002 for these three users" is
  very knowable: the moment we receive EOSE from the indexer relays we use we
  know, one way or another, whether we have a 10002 or not.'

  Settlement is that confirmation and nothing else -- not a timeout, not a
  retry budget, not a heuristic. Until it arrives the write parks; it is never
  failed and its destinations are never guessed.

  Background:
    Given only 2 indexer relays are configured
    And I am logged in as my own account
    And my relay list names "wss://mine.example" as my write relay
    And no relay list for Carol has ever been ingested

  @designed
  Scenario: Sources finish with nothing, so the absence is settled and routing completes
    Given I published a note mentioning Carol while nothing was known about her
    And the note is parked awaiting Carol's relay list
    When both indexers confirm end of stored events having sent no relay list for Carol
    Then the engine knows Carol has no relay list
    And routing for the note is complete
    And no relay is contacted on Carol's behalf

  @designed
  Scenario: One source still unfinished is not a settlement
    Given I published a note mentioning Carol while nothing was known about her
    When one indexer confirms end of stored events having sent no relay list for Carol
    And the other indexer has not confirmed end of stored events
    Then the engine does not claim Carol has no relay list
    And routing for the note is not complete
    And the note stays parked awaiting Carol's relay list
    And the note is not reported as failed

  @designed
  Scenario: Waiting a long time is not a settlement
    Given I published a note mentioning Carol while nothing was known about her
    When neither indexer confirms end of stored events for a long time
    Then the engine does not claim Carol has no relay list
    And routing for the note is not complete
    And no relays are guessed for Carol

  @designed
  Scenario: A late relay list overrides a settled absence while the route is still open
    Given no relay list for Dave has ever been ingested
    And I published a note mentioning Carol and Dave
    And both indexers confirmed end of stored events having sent no relay list for Carol
    And the indexers have not confirmed end of stored events for Dave's relay list
    And routing for the note is not complete
    When Carol's relay list arrives naming "wss://carol-relay.example" as her read relay
    Then the engine knows Carol's read relay is "wss://carol-relay.example"
    And the next resolution of the note publishes it to "wss://carol-relay.example"
    And routing for the note is still not complete
    When both indexers confirm end of stored events having sent no relay list for Dave
    Then routing for the note is complete

  @designed
  Scenario: A relay list arriving after the route retired does not reopen it
    Given I published a note mentioning Carol
    And both indexers confirmed end of stored events having sent no relay list for Carol
    And routing for the note is complete
    When Carol's relay list arrives naming "wss://carol-relay.example" as her read relay
    Then the engine knows Carol's read relay is "wss://carol-relay.example"
    And the note is not published to "wss://carol-relay.example"
    And routing for the note stays complete
    But a note I publish mentioning Carol afterwards is published to "wss://carol-relay.example"
