Feature: Replaceable writes keep only useful delivery work
  @ledger-9
  Scenario Outline: A newer offline replaceable write retires the older obligation
    Given my relay list names "offline-relay" as my write relay
    And I am logged in as my own account
    When relay "offline-relay" drops the connection
    And I publish kind <kind> with d tag "<d>" saying "older"
    Then the first receipt reports waiting for "offline-relay"
    When I publish kind <kind> with d tag "<d>" saying "newer"
    Then the first receipt reports superseded by the newer replaceable write
    And the second receipt reports waiting for "offline-relay"
    When relay "offline-relay" comes back
    Then the second receipt reports acked by "offline-relay"

    Examples:
      | kind  | d        |
      | 0     | ignored  |
      | 3     | ignored  |
      | 10001 | ignored  |
      | 30001 | presence |
