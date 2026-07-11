Feature: Limits are bounded and explicit
  @ledger-17 @wip
  Scenario: A slow query observer eventually receives exact latest local state
    Given a live kind 9999 query and a deliberately stalled observer
    When a relay delivers a burst larger than the observer buffer
    Then observation memory remains within its configured bound
    And the next delivered snapshot contains every accepted local mutation
    And diagnostics reports the coalesced intermediate frame count

  @ledger-4 @ledger-17 @wip
  Scenario: An impossible relay objective reports shortfall
    Given the query requests two-relay coverage for every selected author
    And available routing facts cannot satisfy that objective under the cap
    When I observe the query snapshot
    Then cached matching rows remain available
    And shortfall evidence names the uncovered demand
    And no complete-acquisition claim is emitted

  @ledger-17 @wip
  Scenario: Derived cardinality is never silently truncated
    Given a derived binding resolves beyond the configured graph limit
    When I observe the outer query
    Then NMP either chunks the complete set exactly or reports shortfall
    And it never substitutes an unexplained first-N set
