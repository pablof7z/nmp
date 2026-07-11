Feature: Protocol modules compose without stealing schema ownership
  @ledger-3 @ledger-14 @wip
  Scenario: NIP-29 publishes a NIP-68 photo into a hosted group
    Given the NIP-68 module built an immutable unsigned photo draft
    And the NIP-29 group is hosted by relay "group-host"
    When I publish the photo through that group
    Then NIP-29 adds the correct group h-tag to a new draft
    And NIP-68 remains the owner of the photo schema
    And diagnostics attributes relay "group-host" to typed group context
    And core validates the final body and signs it exactly once

  @ledger-14 @wip
  Scenario: A module cannot claim an event schema its NIP does not define
    Given NIP-29 does not define the photo event schema
    When NIP-29 attempts to register ownership of that photo kind
    Then engine construction fails with a schema ownership error
