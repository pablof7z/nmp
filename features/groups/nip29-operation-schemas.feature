Feature: NIP-29's own operations draft their exact kind and tag schema
  Join, leave and moderation are kinds NIP-29 itself defines
  (`crates/nmp-nip29/src/operations.rs`). Each composer returns a plain draft
  -- no pubkey, no signature, no group context -- so this domain is provable
  entirely on its own, independent of which door later appends the group's
  `h` tag and routes the draft to a host. That combination is a separate
  distinction, covered under `PROTOCOL-GROUPISANIDENTITY-*` and
  `PROTOCOL-NIP29OPERATIONS-009/010/012/013` in
  `features/groups/one-typed-group-door.feature`.

  Traces to docs/internals/nip29/group-publication.md section 7, and
  supersedes the un-executed five-row outline in the legacy
  `features/groups/nip29-operations.feature` fixture for the nine schema
  distinctions proved here.

  # nmp:id=NIP29OPERATIONS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::join_request_with_code_carries_kind_h_free_code_tag
  # nmp:falsifier=Changing the join-request kind constant away from 9021 makes the exact owner proof fail.
  @nip29
  Scenario: A join request with an invite code drafts kind 9021 and a code tag
    When I compose a join request with invite code "dark-slide-42"
    Then the draft is kind 9021
    And the draft carries a code tag with value "dark-slide-42"
    And the draft carries no h tag

  # nmp:id=NIP29OPERATIONS-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::join_request_with_no_code_carries_no_code_tag_and_no_empty_tag
  # nmp:falsifier=Making the join-request composer always attach a code tag, even when none was supplied, makes the exact owner proof fail.
  @nip29
  Scenario: A join request with no invite code carries no code tag
    When I compose a join request with no invite code
    Then the draft is kind 9021
    And the draft carries no code tag
    And the draft carries no tag at all

  # nmp:id=NIP29OPERATIONS-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::leave_request_is_kind_9022_with_no_tags
  # nmp:falsifier=Changing the leave-request kind constant away from 9022 makes the exact owner proof fail.
  @nip29
  Scenario: A leave request drafts kind 9022 with no tags
    When I compose a leave request
    Then the draft is kind 9022
    And the draft carries no tag at all

  # nmp:id=NIP29OPERATIONS-004
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::add_user_carries_kind_9000_and_a_bare_p_tag
  # nmp:falsifier=Changing the add-user kind constant away from 9000 makes the exact owner proof fail.
  @nip29
  Scenario: Adding a user with no role drafts kind 9000 and a bare p tag
    When I compose adding user "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" to the group with no role
    Then the draft is kind 9000
    And the draft carries a p tag naming "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"
    And the p tag carries no role value

  # nmp:id=NIP29OPERATIONS-005
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::add_user_with_role_carries_the_role_on_the_p_tag
  # nmp:falsifier=Dropping the role value from the composed p tag makes the exact owner proof fail.
  @nip29
  Scenario: Adding a user with a role carries the role on the p tag
    When I compose adding user "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" to the group with role "moderator"
    Then the draft carries a p tag naming "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" with role "moderator"

  # nmp:id=NIP29OPERATIONS-006
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::remove_user_carries_kind_9001_and_a_p_tag
  # nmp:falsifier=Changing the remove-user kind constant away from 9001 makes the exact owner proof fail.
  @nip29
  Scenario: Removing a user drafts kind 9001 and a p tag
    When I compose removing user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then the draft is kind 9001
    And the draft carries a p tag naming "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad"

  # nmp:id=NIP29OPERATIONS-007
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::edit_metadata_carries_both_fields_when_both_are_supplied
  # nmp:falsifier=Dropping the about tag when both fields were supplied makes the exact owner proof fail.
  @nip29
  Scenario: Editing both metadata fields drafts kind 9002 with both tags
    When I compose editing the group metadata with name "Photographers" and about "film only, no spoilers"
    Then the draft is kind 9002
    And the draft carries a name tag with value "Photographers"
    And the draft carries an about tag with value "film only, no spoilers"

  # nmp:id=NIP29OPERATIONS-008
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::edit_metadata_editing_one_field_leaves_the_other_untouched
  # nmp:falsifier=Emitting an empty about tag when about was not supplied makes the exact owner proof fail.
  @nip29
  Scenario: Editing only one metadata field leaves the other field's tag absent
    When I compose editing the group metadata with name "Photographers" and nothing else
    Then the draft carries a name tag with value "Photographers"
    And the draft carries no about tag
    And the draft carries no empty tag

  # nmp:id=NIP29OPERATIONS-011
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::join_request_with_code_carries_kind_h_free_code_tag
  # nmp:evidence=rust:nmp-nip29::leave_request_is_kind_9022_with_no_tags
  # nmp:evidence=rust:nmp-nip29::add_user_carries_kind_9000_and_a_bare_p_tag
  # nmp:evidence=rust:nmp-nip29::remove_user_carries_kind_9001_and_a_p_tag
  # nmp:evidence=rust:nmp-nip29::edit_metadata_carries_both_fields_when_both_are_supplied
  # nmp:evidence=rust:nmp-nip29::delete_event_carries_kind_9005_and_an_e_tag
  # nmp:evidence=rust:nmp-nip29::create_group_is_kind_9007_with_no_tags
  # nmp:evidence=rust:nmp-nip29::delete_group_is_kind_9008_with_no_tags
  # nmp:evidence=rust:nmp-nip29::create_invite_carries_kind_9009_and_the_code_tag
  # nmp:falsifier=Changing any one of the nine operation kind constants makes its exact owner proof fail; the performed mutation moved create-invite's 9009 to 9010.
  @nip29
  Scenario Outline: All nine of NIP-29's own operations are named, exhaustively, at their defined kind
    When I compose the group operation <operation>
    Then the draft is kind <kind>

    Examples:
      | operation      | kind |
      | join request   | 9021 |
      | leave request   | 9022 |
      | add user        | 9000 |
      | remove user       | 9001 |
      | edit metadata       | 9002 |
      | delete event         | 9005 |
      | create group          | 9007 |
      | delete group           | 9008 |
      | create invite           | 9009 |
