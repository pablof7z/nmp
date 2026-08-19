Feature: The h tag belongs to the group, not to the caller
  An app hands the group an event; it does not hand the group its own opinion
  about which group that event is in. That opinion is refused with a typed
  error, and refused before signing, so a rejected publication leaves no
  signature and no journal row behind.

  Traces to docs/internals/nip29/group-publication.md section 5.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  @nip29
  Scenario: An event already carrying this group's own h is still refused
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-h error
    And the error names the h tag
    And relay "wss://relay.groups.example" received no event
    And the signer was never asked to sign
    And no write intent was accepted

  @nip29
  Scenario: An event carrying another group's h is refused the same way
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "darkroom"
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-h error
    And the refusal is the same error as for a matching h
    And relay "wss://relay.groups.example" received no event

  @nip29
  Scenario: A refused publication is distinguishable from a rejected one
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I publish that event through the group
    Then the refusal is reported as a caller error, not as a relay rejection
    And no receipt was created for it
