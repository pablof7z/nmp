Feature: An accepted unchanged request is not sent twice
  An accepted request owns its exact subscription id, filter, and transport
  generation until it is replaced, closed, or disconnected. Recompiling the
  same plan therefore leaves the wire quiet; reconnecting a fresh generation
  replays the current request exactly once.

  Background:
    Given I am logged in as my own account
    And relay "hub" is the relay I watch directly
    # This feature isolates the ordinary NIP-01 router plan. A behaviorally
    # proven NIP-77 relay deliberately opens a distinct `limit:0` live
    # candidate, waits for its EOSE, and only then overlap-closes the prior
    # REQ.
    And relay "hub" advertises that NIP-77 is unsupported

  # nmp:id=ROUTING-SUBSCRIPTIONCOLLAPSE-020
  # nmp:status=built
  # nmp:evidence=rust:nmp::unchanged_same_generation_req_is_suppressed_and_reconnect_replays_once
  # nmp:falsifier=forcing the exact accepted-request predicate false makes the independent relay witness observe a byte-identical duplicate
  Scenario: Nothing already asked for is asked for again
    # `EngineCore` owns the exact accepted request on one transport generation.
    # A byte-identical request therefore mints neither another request
    # incarnation nor another wire frame. A changed filter remains a real
    # replacement, and a fresh generation receives the current request once.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" was never asked for the same thing twice
