Feature: A capability can define the first complete value without claiming relay absence
  A replaceable operation normally starts from a complete event already known
  to NMP. When no event is known, a configured capability may instead define
  its own complete empty value for the requested coordinate. That local start
  is not evidence that relays have no value. NMP keeps the source unresolved,
  retains the operation across restart, and reapplies it if relay truth later
  arrives.

  @acceptance
  Scenario: A capability creates the first value without pretending the network proved absence
    Given no relay event is known for a replaceable coordinate
    When its configured capability applies an operation to its own empty value
    Then one complete replacement enters ordinary custody immediately
    And its source evidence remains unresolved rather than absent
    And restart preserves that complete generation and the same receipt
    When a relay value is durably observed while the engine is closed
    And NMP reopens without another capability action
    Then NMP reapplies the retained operation over the relay value
    And the successor preserves fields the capability operation does not own
    And the same mechanism works for ordinary and parameterized coordinates
