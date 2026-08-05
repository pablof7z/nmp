Feature: The retained scope supplies the h rows, at either arity
  #1281 and #1283 are one door seen twice. Both ask what the RETAINED scope
  supplies to a write the app is assembling itself, and before this it
  supplied it in exactly one shape: `Group::intent`, one group, NMP signs.

  #1283 is the missing half of that. An app that signs its own bytes -- any
  app that shows a message the moment it is composed, because an event id
  only exists once the body is frozen -- needed the `h` row INSIDE the bytes
  it signs, and `signed_intent` deliberately validates rather than appends.
  So it reached past the facade for `nmp_nip29::contextualize(group_id, ...)`
  and then handed the result back to `group.signed_intent(...)`, naming the
  group id twice, from two crates, with nothing checking the two agreed until
  the second call. That is the failure the retained id exists to make
  unrepresentable -- only caught, and caught one signature too late.

  #1281 is the same question at a larger arity. A kind:30315 session status
  is addressable at `(author, d=status)` and carries one `h` per room the
  session occupies, so publishing it once per room makes each copy REPLACE
  the last: a multi-`h` write is not a convenience, it is the only correct
  shape for that event. No door minted one, so a real consumer hand-built a
  `WriteIntent` -- spelling its own routing and its own `h` rows -- which is
  precisely what #1242 removed for every other group write.

  Both close as `Groups`: the hosts a scope named plus the SET of ids one
  event claims, with `contextualize`/`intent`/`signed_intent` on it, and
  `Group`'s write half is literally the one-element case of it.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::one_intent_carries_every_group_and_the_apps_own_coordinate
  # nmp:evidence=rust:nmp-nip29::a_draft_for_several_groups_carries_one_h_row_per_group
  # nmp:falsifier=appending only the first id, or minting one intent per id, makes one_intent_carries_every_group_and_the_apps_own_coordinate see fewer h rows than rooms and makes a_draft_for_several_groups_carries_one_h_row_per_group observe a shorter row list
  @nip29
  Scenario: One event, several groups, one intent
    When I mint a group write intent for kind 30315 addressed to "darkroom" and "photographers"
    Then the minted intent carries one h row per named group
    And the minted intent routes to "wss://relay.groups.example" and to no other relay
    And the app named no relay and no h row

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::publishing_once_per_room_would_share_one_replaceable_coordinate
  # nmp:falsifier=publishing an addressable status once per room instead of once with several h rows makes publishing_once_per_room_would_share_one_replaceable_coordinate observe two DIFFERENT (kind, author, d) coordinates, which would mean the per-room copies do not in fact replace each other and the multi-h shape was never needed
  @nip29
  Scenario: Publishing once per room would collide on one replaceable coordinate
    When I mint a group write intent for kind 30315 addressed to "darkroom" and "photographers"
    Then the same status published once per room would share one addressable coordinate

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::the_composed_rows_do_not_depend_on_the_callers_own_order_or_repetition
  # nmp:evidence=rust:nmp::duplicate_and_unsorted_ids_canonicalize_to_one_set
  # nmp:falsifier=retaining the caller's own iteration order (a Vec rather than a canonical set) makes the_composed_rows_do_not_depend_on_the_callers_own_order_or_repetition observe two different row orders and makes duplicate_and_unsorted_ids_canonicalize_to_one_set observe two unequal values
  @nip29
  Scenario: The composed bytes do not depend on how the caller spelled the set
    When I mint a group write intent for kind 30315 addressed to "darkroom" and "photographers"
    Then naming the same rooms in another order composes the identical rows

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-004
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_signed_event_for_several_groups_must_name_exactly_those_groups
  # nmp:evidence=rust:nmp::a_signed_write_missing_one_room_is_refused_not_narrowed
  # nmp:falsifier=accepting a pre-signed event whose h rows are a SUBSET of the named rooms makes a_signed_event_for_several_groups_must_name_exactly_those_groups and a_signed_write_missing_one_room_is_refused_not_narrowed observe Ok instead of MismatchedContext -- a status silently narrowed to fewer rooms stops rendering where the app believes it still shows
  @nip29
  Scenario: A pre-signed multi-group write must name exactly the rooms it is published into
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    When I publish that signed event through the groups "darkroom" and "photographers"
    Then the publication is refused with a typed mismatched-group-context error
    And relay "wss://relay.groups.example" received no event

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_write_context_over_no_group_is_never_formed
  # nmp:falsifier=forming a write context over an empty id set and refusing later (or never) makes a_write_context_over_no_group_is_never_formed observe an Ok value instead of NoGroupNamed, which would let an h-less event be routed as a group write
  @nip29
  Scenario: A write context over no group is never formed at all
    Then naming no group at all forms no write context

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_self_signed_write_is_composed_and_minted_from_the_same_retained_id
  # nmp:evidence=rust:nmp::a_self_signed_write_names_the_rooms_once_and_mints_from_the_same_value
  # nmp:falsifier=deleting Group::contextualize so an app must call nmp_nip29::contextualize with a group id of its own makes a_self_signed_write_is_composed_and_minted_from_the_same_retained_id and a_self_signed_write_names_the_rooms_once_and_mints_from_the_same_value fail to compile against the retained value alone -- the two-crate, twice-spelled path is exactly what #1283 reported
  @nip29
  Scenario: An app that signs its own bytes never spells the group id
    When I contextualise a draft of kind 9 through the group and sign it myself
    Then the draft I signed already carried this group's h row
    And publishing it back through the group keeps the id I already had

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_self_signing_door_refuses_a_caller_supplied_row_exactly_as_the_mint_door_does
  # nmp:evidence=rust:nmp-nip29::a_caller_supplied_row_is_refused_at_the_several_group_arity_too
  # nmp:falsifier=letting contextualize pass a draft that already carries an h row (on the grounds that the caller is signing it anyway) makes the_self_signing_door_refuses_a_caller_supplied_row_exactly_as_the_mint_door_does and a_caller_supplied_row_is_refused_at_the_several_group_arity_too observe Ok instead of CallerSuppliedContext, reopening the ownership hole on the self-signing path alone
  @nip29
  Scenario: The self-signing door is not a laxer back door into the h row
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I contextualise that draft through the group
    Then the publication is refused with a typed caller-supplied-h error

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-008
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_is_the_one_element_case_of_a_several_group_write
  # nmp:falsifier=giving Group its own contextualize/mint implementation rather than delegating to the one-element Groups makes a_group_write_is_the_one_element_case_of_a_several_group_write observe a difference in route, identity, correlation or composed tags between the two doors
  @nip29
  Scenario: One group is the one-element case, in the code and not only in the prose
    When I mint a group write intent for kind 9 addressed to "photographers" alone
    Then it is byte-identical to what the one-group door mints
