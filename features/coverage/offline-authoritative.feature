Feature: Offline cache rows keep their scoped acquisition evidence
  @ledger-7 @wip
  Scenario: Relaunch returns cached rows without claiming global truth
    Given a previous session cached matching kind 9999 events from two planned relays
    And one relay had finished its request while the other was unavailable
    When I reconstruct the engine with no network
    Then the query immediately shows the cached matching rows
    And its evidence says which planned relay had finished
    And its evidence says the other planned relay is unavailable
    And the query makes no global complete or authoritative-empty claim
