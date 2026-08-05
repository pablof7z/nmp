Feature: One write, several groups, through the unsigned door
  A kind:30315 session status is addressable at `(author, d=status)` and
  carries one `h` per room the session occupies, so the same status renders in
  every room the agent is in. Publishing it once per room makes each copy
  REPLACE the last -- they share the coordinate -- so a multi-`h` write is not
  a convenience, it is the only correct shape for that event.

  `Group` is one relay scope plus one group id, and both its mint doors
  enforced that, so this write had no door at all. The consumer that needed it
  hand-built a `WriteIntent`, spelling its own `WriteRouting::Explicit` and
  writing its own `h` rows -- exactly what #1242 removed for every other group
  write, and the last piece of protocol logic left in that app.

  `Groups` closes it: the hosts a scope named plus the SET of ids one event
  claims, with `intent` and `publish` and nothing else. Both are the UNSIGNED
  door -- NMP appends the rows, NMP signs, and the app reads its own write back
  through the subscription it already holds.

  There is deliberately NO pre-signed several-group door. It would be easy to
  assume one is needed, because the consumer reached the multi-`h` shape by
  routing status through a pre-signed path. Its own source says otherwise: the
  unsigned path already computed the event id without signing (an id is a hash
  of `(author, created_at, kind, tags, content)` and a signature was never an
  input), and the refusal that pushed status onto the signed path said, in so
  many words, that exact multi-group events must be pre-signed. That was an
  ARITY limit, not an id-timing requirement, and this is what removes it.

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
  # nmp:evidence=rust:nmp::a_write_context_over_no_group_is_never_formed
  # nmp:falsifier=forming a write context over an empty id set and refusing later (or never) makes a_write_context_over_no_group_is_never_formed observe an Ok value instead of NoGroupNamed, which would let an h-less event be routed as a group write
  @nip29
  Scenario: A write context over no group is never formed at all
    Then naming no group at all forms no write context

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-005
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_caller_supplied_row_is_refused_at_the_several_group_arity_too
  # nmp:evidence=rust:nmp::a_caller_supplied_context_is_refused_before_any_several_group_intent_exists
  # nmp:falsifier=letting the several-group door accept a draft that already carries an h row makes a_caller_supplied_row_is_refused_at_the_several_group_arity_too and a_caller_supplied_context_is_refused_before_any_several_group_intent_exists observe Ok instead of CallerSuppliedContext -- the ownership rule would then hold at one arity and not the other, which is the shape of hole that let a real consumer route around it
  @nip29
  Scenario: The h row belongs to the retained scope at this arity too
    Given an unsigned event of kind 30315 with content "working"
    And that event already carries an h tag with value "photographers"
    When I mint a group write intent for that event addressed to "darkroom" and "photographers"
    Then the publication is refused with a typed caller-supplied-h error
    And minting took no receipt and reached no relay

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_is_the_one_element_case_of_a_several_group_write
  # nmp:falsifier=giving Group its own mint implementation rather than assembling every group intent through the one-element Groups makes a_group_write_is_the_one_element_case_of_a_several_group_write observe a difference in route, identity, correlation or composed tags between the two doors
  @nip29
  Scenario: One group is the one-element case, in the code and not only in the prose
    When I mint a group write intent for kind 9 addressed to "photographers" alone
    Then it is byte-identical to what the one-group door mints

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_multi_group_write_is_an_ordinary_tracked_write
  # nmp:evidence=rust:nmp::the_inline_door_reaches_the_one_publish_door
  # nmp:falsifier=giving the several-group door a write lifecycle of its own -- a second publish path, a group-shaped receipt, or a correlation it invents rather than accepts -- makes a_multi_group_write_is_an_ordinary_tracked_write fail to recover the write by the app's own token
  @nip29
  Scenario: A several-group write is an ordinary tracked write, with no lifecycle of its own
    When I mint a group write intent for kind 30315 addressed to "darkroom" and "photographers"
    And I stamp my own correlation token on it and publish it
    Then the write is recoverable by that token
