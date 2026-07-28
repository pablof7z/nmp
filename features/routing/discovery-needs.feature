Feature: Parked writes feed the engine's own discovery, never a parallel one
  Pablo, correcting an over-reading: "I meant that any routing crate should use
  the querying system to retrieve the data they need!"

  A write that cannot finish routing declares what it is missing, and the
  engine folds that into the relay-list discovery it already runs for reading.
  One subscription, widened; not a second query path alongside it. Everything
  that follows -- deduplication across writes, sharing with an open feed,
  teardown when nobody needs an entry any more -- falls out of that single
  mechanism rather than being arranged separately.

  Background:
    Given only 2 indexer relays are configured
    And I am logged in as my own account
    And my relay list names "wss://mine.example" as my write relay

  @designed
  Scenario: A parked write's missing author joins the discovery already running
    Given no relay list for Carol has ever been ingested
    When I publish a note mentioning Carol
    Then the indexers are asked only for relay lists
    And the engine's existing relay-list discovery now covers Carol
    And no subscription is opened outside the engine's own discovery

  @designed
  Scenario: Two writes missing the same author cost one fetch
    Given no relay list for Carol has ever been ingested
    When I publish a note mentioning Carol
    And I publish a second note mentioning Carol
    Then Carol's relay list is requested exactly once
    And both notes are parked awaiting the same answer
    When both indexers confirm end of stored events having sent no relay list for Carol
    Then routing for both notes is complete

  @designed
  Scenario: A write needing what an open feed already needs adds no request
    Given I am logged in as an account that follows Carol
    And my feed of my follows' notes is open and already needs Carol's relay list
    When I publish a note mentioning Carol
    Then no additional request for Carol's relay list is made
    And the note is parked awaiting the answer the feed is already waiting for

  @designed
  Scenario: A discovery entry is torn down when nothing needs it any more
    Given no relay list for Carol has ever been ingested
    And no open query needs Carol's relay list
    And I published a note mentioning Carol
    And the engine's existing relay-list discovery covers Carol
    When both indexers confirm end of stored events having sent no relay list for Carol
    And routing for the note is complete
    Then the engine's relay-list discovery no longer covers Carol
    And the indexers are not asked for Carol's relay list again

  @designed
  Scenario: The absence is derived once and read afterwards as an ordinary fact
    Given both indexers confirmed end of stored events having sent no relay list for Carol
    When I publish a note mentioning Carol
    Then routing for the note is complete immediately
    And no request for Carol's relay list is made
    And the write plane is told only that Carol has no relay list

  @designed
  Scenario: A recovered write declares its missing author again
    Given I published a note mentioning Carol and the process stopped while it was parked
    When I reconstruct the engine from the same durable store
    Then the engine's existing relay-list discovery covers Carol again
    And no discovery state was read back from the journal to make that happen
