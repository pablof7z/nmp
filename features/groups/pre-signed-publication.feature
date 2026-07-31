Feature: A pre-signed event is published unchanged, and its h is validated
  Some apps sign first, take the exact event id, arm an observation on it, and
  only then publish. That path cannot append anything: appending an h would
  change the bytes and therefore the id. So on the pre-signed path the group
  VALIDATES the h that is already there. A missing or wrong h is a typed
  refusal -- never a silent repair, never a re-sign.

  Traces to docs/internals/nip29/group-publication.md section 6.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_pre_signed_event_is_carried_into_the_minted_intent_byte_for_byte
  # nmp:evidence=rust:nmp::publish_signed_delivers_the_callers_exact_pre_signed_bytes_to_every_host
  # nmp:falsifier=mutating any field of a pre-signed event before publish (a tag, the content, the id) makes a_pre_signed_event_is_carried_into_the_minted_intent_byte_for_byte's payload-equality assertion and publish_signed_delivers_the_callers_exact_pre_signed_bytes_to_every_host's delivered-event assertion fail
  @nip29
  Scenario: A correctly contextualised signed event goes out byte for byte
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    And that signed event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    When I publish that signed event through the group
    Then the delivered event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And its signature is byte-identical to the one I supplied
    And no tag was added, removed or reordered
    And the signer was never asked to sign
    And it was delivered to "wss://relay.groups.example" and to no other relay

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_pre_signed_event_s_own_id_is_known_before_publication_and_matches_a_live_query_armed_on_it
  # nmp:falsifier=arming the live query on a fixture label instead of the event's own real, dynamically computed id makes the_pre_signed_event_s_own_id_is_known_before_publication_and_matches_a_live_query_armed_on_it never observe a match
  @nip29
  Scenario: The id is known before publication, so an observation can be armed on it
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    And that signed event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And I am observing a live query for exactly that id
    When I publish that signed event through the group
    Then the query for that id matches the event that reached "wss://relay.groups.example"

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_signed_event_with_no_context_is_refused_not_repaired
  # nmp:falsifier=appending an h row to a signed event with none instead of refusing it makes a_signed_event_with_no_context_is_refused_not_repaired observe Ok instead of MissingContext
  @nip29
  Scenario: A signed event with no h is refused, not repaired
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries no h tag
    When I publish that signed event through the group
    Then the publication is refused with a typed missing-group-context error
    And no h tag was appended to it
    And its id was never recomputed
    And relay "wss://relay.groups.example" received no event

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-004
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_signed_event_naming_another_group_names_both_in_its_refusal
  # nmp:falsifier=silently accepting a signed event whose h names a different group instead of refusing it makes a_signed_event_naming_another_group_names_both_in_its_refusal observe Ok instead of MismatchedContext
  @nip29
  Scenario: A signed event carrying another group's h is refused, and the error says both
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "still wet"
    And that signed event carries an h tag with value "darkroom"
    When I publish that signed event through the group "photographers"
    Then the publication is refused with a typed mismatched-group-context error
    And the error names both "darkroom" and "photographers"
    And no relay received the event

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-005
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_signed_event_with_two_context_rows_is_ambiguous
  # nmp:falsifier=picking the first h row instead of refusing a signed event with two h rows makes a_signed_event_with_two_context_rows_is_ambiguous observe Ok instead of AmbiguousContext
  @nip29
  Scenario: A signed event with more than one h tag is refused
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries h tags with values "photographers" and "darkroom"
    When I publish that signed event through the group
    Then the publication is refused with a typed ambiguous-group-context error
    And relay "wss://relay.groups.example" received no event

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_route_follows_the_group_not_whichever_key_signed_the_pre_signed_event
  # nmp:evidence=rust:nmp::a_pre_signed_event_from_another_author_routes_only_to_the_host_never_to_their_own_outbox
  # nmp:falsifier=routing a pre-signed event by its signer's own relay list instead of the group's retained host makes the_route_follows_the_group_not_whichever_key_signed_the_pre_signed_event's route assertion and a_pre_signed_event_from_another_author_routes_only_to_the_host_never_to_their_own_outbox's untouched-outbox assertion fail
  @nip29
  Scenario: The route follows the group, not the signature
    Given an event signed earlier by "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" of kind 9 with content "not mine"
    And that signed event carries an h tag with value "photographers"
    And "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" names "wss://bob-write.example" as their write relay
    When I publish that signed event through the group
    Then it was delivered to "wss://relay.groups.example" and to no other relay
    And relay "wss://bob-write.example" received no event
    And relay "wss://alice-write.example" received no event
    And the signature still belongs to "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"

  # nmp:id=PROTOCOL-PRESIGNEDPUBLICATION-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_host_rejection_of_a_pre_signed_event_is_an_ordinary_receipt_tied_to_its_unchanged_known_id
  # nmp:falsifier=re-signing or re-routing a host-rejected pre-signed event instead of leaving it an ordinary per-relay Rejected fact makes a_host_rejection_of_a_pre_signed_event_is_an_ordinary_receipt_tied_to_its_unchanged_known_id's id/route assertions fail
  @nip29
  Scenario: A pre-signed publication that the host rejects keeps its id
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    And that signed event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And relay "wss://relay.groups.example" rejects every event
    When I publish that signed event through the group
    Then the receipt reports the event rejected by "wss://relay.groups.example"
    And the receipt is addressed by the same id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And the event was not re-signed and not re-routed
