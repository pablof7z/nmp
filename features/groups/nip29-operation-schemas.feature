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

  # nmp:id=NIP29OPERATIONS-012
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::edit_metadata_composes_the_picture_row_and_both_marker_rows
  # nmp:evidence=rust:nmp-ffi::the_metadata_edit_door_composes_the_picture_and_marker_rows
  # nmp:falsifier=Composing only name and about -- the shape kind:9002 had before #1282 -- makes edit_metadata_composes_the_picture_row_and_both_marker_rows observe a two-row draft instead of the four-row one, which is the state that forced a real consumer to hand-write ["closed"] and therefore to hand-assemble a whole 9002.
  @nip29
  Scenario: Editing metadata drafts NIP-29's picture row and its marker rows
    When I compose editing the group metadata to a public closed workspace named "Workspace" pictured at "https://cdn.example/w.png"
    Then the draft is kind 9002
    And the draft carries a name tag with value "Workspace"
    And the draft carries a picture tag with value "https://cdn.example/w.png"
    And the draft carries a bare public tag
    And the draft carries a bare closed tag

  # nmp:id=NIP29OPERATIONS-013
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::each_marker_axis_composes_the_exact_tag_nip29_spells_for_that_state
  # nmp:falsifier=Collapsing the two independent axes onto one setting, or spelling either axis with a single boolean whose false case emits nothing, makes each_marker_axis_composes_the_exact_tag_nip29_spells_for_that_state observe a missing or wrong marker for at least one of the four combinations -- and a public-but-closed group, which is what a published workspace is, would stop being expressible.
  @nip29
  Scenario: Who may read and whether joins are honoured are independent two-valued choices
    Then each of NIP-29's four read/join combinations composes exactly its own two marker tags

  # nmp:id=NIP29OPERATIONS-014
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::an_unstated_marker_emits_no_row_and_therefore_clears_nothing
  # nmp:falsifier=Spelling either marker as a plain boolean that always emits a row makes an_unstated_marker_emits_no_row_and_therefore_clears_nothing observe an edit that renames a group also restating -- and therefore silently resetting -- who may read it and whether it accepts joins.
  @nip29
  Scenario: An unstated marker emits no row, so an edit never clears what it did not mention
    When I compose editing the group metadata with name "Workspace" and nothing else
    Then the draft carries a name tag with value "Workspace"
    And the draft carries no marker tag at all

  # nmp:id=NIP29OPERATIONS-015
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::create_group_under_a_parent_carries_the_parent_row_on_the_create
  # nmp:evidence=rust:nmp-nip29::create_group_at_the_root_is_kind_9007_with_no_tag_at_all
  # nmp:evidence=rust:nmp-nip29::the_parent_row_carries_the_group_id_verbatim
  # nmp:evidence=rust:nmp-ffi::the_create_door_composes_the_parent_row_and_omits_it_for_a_root
  # nmp:falsifier=Dropping the parent argument from create_group -- the shape kind:9007 had before #1301 -- makes create_group_under_a_parent_carries_the_parent_row_on_the_create observe a draft with no rows at all, which is the state that forced a real consumer to append ["parent", id] to NMP's own builder by hand. Emitting the row unconditionally instead makes create_group_at_the_root_is_kind_9007_with_no_tag_at_all observe a root group declaring an empty parent, which the relay refuses outright.
  @nip29
  Scenario: Creating a subgroup drafts NIP-29's parent row on the create itself
    When I compose creating a group under parent "darkroom"
    Then the draft is kind 9007
    And the draft carries a parent tag with value "darkroom"

  # nmp:id=NIP29OPERATIONS-016
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_metadata_edit_never_composes_a_parent_row
  # nmp:falsifier=Adding a parent field to GroupMetadataEdit makes a_metadata_edit_never_composes_a_parent_row observe a parent row on a kind:9002 -- a row the only relay implementing subgroups reads on neither path, so the app would be told it had moved a group under a new parent when the relay had left it exactly where it was.
  @nip29
  Scenario: A metadata edit states no parent, because no relay honours one there
    When I compose editing the group metadata to a public closed workspace named "Workspace" pictured at "https://cdn.example/w.png"
    Then the draft is kind 9002
    And the draft carries no parent tag

  # nmp:id=NIP29OPERATIONS-011
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::join_request_with_code_carries_kind_h_free_code_tag
  # nmp:evidence=rust:nmp-nip29::leave_request_is_kind_9022_with_no_tags
  # nmp:evidence=rust:nmp-nip29::add_user_carries_kind_9000_and_a_bare_p_tag
  # nmp:evidence=rust:nmp-nip29::remove_user_carries_kind_9001_and_a_p_tag
  # nmp:evidence=rust:nmp-nip29::edit_metadata_carries_both_fields_when_both_are_supplied
  # nmp:evidence=rust:nmp-nip29::delete_event_carries_kind_9005_and_an_e_tag
  # nmp:evidence=rust:nmp-nip29::create_group_at_the_root_is_kind_9007_with_no_tag_at_all
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
