Feature: The h tag is inside the bytes that were signed
  Contextualization operates on the unsigned event, before the signing step.
  An h added after signing would change the bytes and therefore the event id,
  so it is never added afterwards -- not as a repair, not as a convenience.

  Traces to docs/internals/nip29/group-publication.md section 5 ("h is appended
  BEFORE signing") and section 6.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  # nmp:id=PROTOCOL-CONTEXTTAGISSIGNED-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::the_builder_handed_onward_for_signing_already_carries_exactly_one_h_row
  # nmp:falsifier=appending the h row after the builder is handed onward instead of before makes the_builder_handed_onward_for_signing_already_carries_exactly_one_h_row see zero h rows on the minted builder
  @nip29
  Scenario: The signer is handed an event that already carries its group context
    When I publish an event of kind 9 with content "first light" through the group
    Then the signer was asked to sign exactly once
    And the event handed to the signer already carried h "photographers"
    And no tag was added to the event after it was signed

  # nmp:id=PROTOCOL-CONTEXTTAGISSIGNED-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::the_delivered_event_s_id_and_signature_cover_the_h_tag
  # nmp:falsifier=comparing a tampered event's id against a freshly recomputed hash instead of the event's own stored id makes tampered.verify() unexpectedly succeed in the_delivered_event_s_id_and_signature_cover_the_h_tag after the h row is removed
  @nip29
  Scenario: The delivered event's id and signature cover the h tag
    When I publish an event of kind 9 with content "first light" through the group
    Then recomputing the event id over the delivered event reproduces the id it was delivered with
    And the signature verifies over those exact bytes
    And removing the h tag from the delivered event changes its id

  # nmp:id=PROTOCOL-CONTEXTTAGISSIGNED-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_signing_failure_leaves_no_event_frame_and_no_delivery_implying_receipt
  # nmp:falsifier=letting a signing failure still emit an EVENT frame or a delivery-implying receipt makes the wire/receipt assertions in a_signing_failure_leaves_no_event_frame_and_no_delivery_implying_receipt fail
  @nip29
  Scenario: A signing failure leaves nothing on the wire
    Given signing fails for this account
    When I publish an event of kind 9 with content "first light" through the group
    Then relay "wss://relay.groups.example" received no event
    And the failure is reported as a signing failure, not as a routing failure
