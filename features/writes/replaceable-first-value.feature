Feature: A capability can define the first complete value without claiming relay absence
  A replaceable operation normally starts from a complete event already known
  to NMP. When no event is known, a configured capability may instead define
  its own complete empty value for the requested coordinate. That local start
  is not evidence that relays have no value. NMP keeps the source unresolved,
  retains the operation across restart, and reapplies it if relay truth later
  arrives.

  # nmp:id=WRITES-REPLACEABLE-EDIT-026
  # nmp:status=built
  # nmp:evidence=rust:nmp::capability_default_survives_restart_and_replays_over_later_source
  # nmp:evidence=rust:nmp-store::capability_default_marker_and_unresolved_source_survive_redb_reopen
  # nmp:falsifier=Treat the capability default as qualified relay absence or fail to retain its starting mode; the store reopen witness loses the unresolved marker and the public successor can no longer preserve the operation and receipt over later relay truth.
  @acceptance
  Scenario: A capability creates the first value without pretending the network proved absence
    Given no relay event is known for a replaceable coordinate
    When its configured capability applies an operation to its own empty value
    Then one complete replacement enters ordinary custody immediately
    And its source evidence remains unresolved rather than absent
    And restart preserves that complete generation and the same receipt
    When a relay later supplies a value for the same coordinate
    Then NMP reapplies the retained operation over the relay value
    And the successor preserves fields the capability operation does not own
    And the same mechanism works for ordinary and parameterized coordinates
