Feature: One engine is one local trust domain
  @wip
  Scenario: Current-pubkey changes do not hide valid cached rows
    Given Alice and Bob are registered in the same engine
    And a valid public kind 9999 event is cached
    When I change the current pubkey from Alice to Bob
    Then every live query whose selection matches the event may still see it

  @wip
  Scenario: Destructive reset prepares the engine for an untrusted local user
    Given the engine contains cached rows, pending writes, receipts, evidence, and signer capabilities
    When the app confirms destructive reset
    Then none of that prior local state remains available
    And the engine can be initialized for another local user
