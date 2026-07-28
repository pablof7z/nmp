Feature: The h tag is inside the bytes that were signed
  Contextualization operates on the unsigned event, before the signing step.
  An h added after signing would change the bytes and therefore the event id,
  so it is never added afterwards -- not as a repair, not as a convenience.

  Traces to docs/internals/nip29/group-publication.md section 5 ("h is appended
  BEFORE signing") and section 6.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  @designed @nip29
  Scenario: The signer is handed an event that already carries its group context
    When I publish an event of kind 9 with content "first light" through the group
    Then the signer was asked to sign exactly once
    And the event handed to the signer already carried h "photographers"
    And no tag was added to the event after it was signed

  @designed @nip29
  Scenario: The delivered event's id and signature cover the h tag
    When I publish an event of kind 9 with content "first light" through the group
    Then recomputing the event id over the delivered event reproduces the id it was delivered with
    And the signature verifies over those exact bytes
    And removing the h tag from the delivered event changes its id

  @designed @nip29
  Scenario: A signing failure leaves nothing on the wire
    Given signing fails for this account
    When I publish an event of kind 9 with content "first light" through the group
    Then relay "wss://relay.groups.example" received no event
    And the failure is reported as a signing failure, not as a routing failure
