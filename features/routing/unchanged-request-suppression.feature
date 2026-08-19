Feature: An accepted unchanged request is not sent twice
  An accepted request owns its exact subscription id, filter, and transport
  generation until it is closed or disconnected. An exact unchanged plan
  leaves the wire quiet; uncovered later work opens an immutable sibling.
  Reconnecting a fresh generation replays each current request exactly once.

  Background:
    Given I am logged in as my own account
    And relay "hub" is the relay I watch directly

  # nmp:id=ROUTING-SUBSCRIPTIONCOLLAPSE-020
  # nmp:status=built
  # nmp:evidence=rust:nmp::accepted_requests_are_immutable_and_reconnect_replays_each_once
  # nmp:falsifier=forcing the exact accepted-request predicate false makes the independent relay witness observe a byte-identical duplicate
  Scenario: Nothing already asked for is asked for again
    # `EngineCore` owns the exact accepted request on one transport generation.
    # A byte-identical request therefore mints neither another request
    # incarnation nor another wire frame. Later uncovered demand opens an
    # immutable sibling, and a fresh generation receives each current request once.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" was never asked for the same thing twice
