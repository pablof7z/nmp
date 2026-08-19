Feature: Diagnostics changes are delivered without rebuilding every close
  Diagnostics describe current reducer state, but ordinary lifecycle changes
  do not need to materialize the same large snapshot repeatedly. NMP marks the
  state dirty, coalesces nearby changes behind one short delivery boundary,
  and still gives a newly attached observer the truthful current snapshot
  immediately.

  Scenario: A close burst produces bounded latest diagnostics
    Given many independent physical requests are active
    And at least one diagnostics observer is attached
    When those requests close in one burst
    Then each close marks diagnostics dirty without rebuilding the full snapshot
    And the first change anchors one bounded delivery window
    And one latest truthful snapshot is delivered to every observer at the boundary
    And no observer receives an intermediate stale snapshot

  Scenario: A new observer gets current truth without a duplicate deadline frame
    Given diagnostics changed and a bounded delivery is pending
    When a new diagnostics observer attaches before the deadline
    Then it receives the current full snapshot immediately
    And that immediate snapshot satisfies the pending delivery
    And neither existing nor new observers receive an unchanged duplicate at the old deadline

  Scenario: Unobserved request terminals update state without building diagnostics
    Given no diagnostics observer is attached
    When an ordinary EOSE changes diagnostic state
    Then the reducer updates its truthful state immediately
    And it marks diagnostics dirty without materializing a snapshot
    And repeated changes arm no delivery work while there are no observers
    And a later observer receives one current snapshot through the bounded delivery contract

  Scenario: A large stalled queue is not ordinary write overhead
    Given many durable writes are stalled without physical relay lanes
    And their bounded diagnostic projection is current
    When one unrelated healthy write is scheduled
    Then only that write's physical lane is read
    And the stalled queue is neither retried nor deleted
    And unrelated diagnostics reuse the current bounded stalled-write projection
    And a real stalled-stage change marks diagnostics dirty before rebuilding it once
