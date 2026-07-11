Feature: Fancy sync only where it is proven to work
  @ledger-8 @wip
  Scenario: An unprobed relay gets a plain subscription
    # Genuine gap, reported rather than faked green (approach doc Appendix
    # item 5): "confirmed to support reconciliation" vs. "never probed" is
    # engine-side state (a `ProbedRelay` capability token earned by a
    # completed NEG-OPEN/NEG-MSG round-trip -- see
    # `nmp-engine/src/negentropy/mod.rs`), not a relay-side toggle
    # `ScriptedRelay` can stage as a `Given`: the underlying `LocalRelay`
    # always answers NEG-OPEN correctly regardless of any config here.
    # Proving this scenario for real means driving one full probe
    # round-trip through the world BEFORE its own `Given`s take effect,
    # which this foundation doesn't yet wire up.
    Given relay "modern-relay" supports reconciliation
    And relay "legacy-relay" has never been probed for reconciliation
    And both serve authors I follow
    When I open a feed of my follows' notes
    Then reconciliation is used with "modern-relay"
    And "legacy-relay" receives a plain subscription
