Feature: One write, several groups, through the one publish door
  A kind:30315 session status is addressable at `(author, d=status)` and
  carries one `h` per room the session occupies, so the same status renders in
  every room the agent is in. Publishing it once per room makes each copy
  REPLACE the last -- they share the coordinate -- so a multi-`h` write is not
  a convenience, it is the only correct shape for that event.

  `Group` is one relay scope plus one group id, and its doors enforced that, so
  this write had no door at all. The consumer that needed it hand-built a
  `WriteIntent`, spelling its own `WriteRouting::Explicit` and writing its own
  `h` rows -- exactly what #1242 removed for every other group write, and the
  last piece of protocol logic left in that app.

  `Groups` closes it: the hosts a scope named plus the SET of ids one event
  claims, with ONE method, `publish`. NMP appends the rows, NMP signs, NMP
  publishes, and the app reads its own write back through the subscription it
  already holds.

  There is deliberately no pre-signed door and no mint-without-publish door.
  The pre-signed one is unnecessary: the consumer's unsigned path already
  computed the event id without signing (an id is a hash of
  `(author, created_at, kind, tags, content)` and a signature was never an
  input), and the refusal that pushed status onto its signed path said, in so
  many words, that exact multi-group events must be pre-signed -- an ARITY
  limit, not an id-timing requirement. The mint-without-publish one buys
  nothing: a `WriteIntent` derives nothing at all, so it cannot be persisted
  across a restart, cloned for batching, or inspected. An app that wants NMP
  to sign without publishing uses the engine's own sign-only door.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_draft_for_several_groups_carries_one_h_row_per_group
  # nmp:evidence=rust:nmp::the_door_contextualizes_with_the_whole_retained_set
  # nmp:falsifier=appending only the first id, or contextualizing with a subset of the retained set, makes a_draft_for_several_groups_carries_one_h_row_per_group observe a shorter row list and makes the_door_contextualizes_with_the_whole_retained_set see fewer h rows than rooms
  @nip29
  Scenario: One event carries one h row per named group
    When I publish an event of kind 30315 into the groups "darkroom" and "photographers"
    Then the delivered event carries one h row per named group
    And the app named no relay and no h row

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::publishing_once_per_room_would_share_one_replaceable_coordinate
  # nmp:falsifier=publishing an addressable status once per room instead of once with several h rows makes publishing_once_per_room_would_share_one_replaceable_coordinate observe two DIFFERENT (kind, author, d) coordinates, which would mean the per-room copies do not in fact replace each other and the multi-h shape was never needed
  @nip29
  Scenario: Publishing once per room would collide on one replaceable coordinate
    Then the same status published once per room would share one addressable coordinate

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::the_composed_rows_do_not_depend_on_the_callers_own_order_or_repetition
  # nmp:evidence=rust:nmp::duplicate_and_unsorted_ids_canonicalize_to_one_set
  # nmp:falsifier=retaining the caller's own iteration order (a Vec rather than a canonical set) makes the_composed_rows_do_not_depend_on_the_callers_own_order_or_repetition observe two different row orders and makes duplicate_and_unsorted_ids_canonicalize_to_one_set observe two unequal values
  @nip29
  Scenario: The composed bytes do not depend on how the caller spelled the set
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
  # nmp:evidence=rust:nmp::a_caller_supplied_context_is_refused_before_any_several_group_write_is_accepted
  # nmp:falsifier=letting the several-group door accept a draft that already carries an h row makes a_caller_supplied_row_is_refused_at_the_several_group_arity_too and a_caller_supplied_context_is_refused_before_any_several_group_write_is_accepted observe Ok instead of CallerSuppliedContext -- the ownership rule would then hold at one arity and not the other, which is the shape of hole that let a real consumer route around it
  @nip29
  Scenario: The h row belongs to the retained scope at this arity too
    Given an unsigned event of kind 30315 with content "working"
    And that event already carries an h tag with value "photographers"
    When I publish that event into the groups "darkroom" and "photographers"
    Then the publication is refused with a typed caller-supplied-h error
    And no relay received the event

  # nmp:id=PROTOCOL-ONEWRITESEVERALGROUPS-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_several_group_write_is_an_ordinary_tracked_write
  # nmp:evidence=rust:nmp::a_several_group_write_routes_to_every_host_in_the_scope
  # nmp:falsifier=giving the several-group door a write lifecycle of its own -- a second publish path, a group-shaped receipt, or a route it derives rather than mints from the retained scope -- makes a_several_group_write_is_an_ordinary_tracked_write fail to find its receipt id in the real publish queue, and makes a_several_group_write_routes_to_every_host_in_the_scope observe a route that is not the scope's whole host set
  @nip29
  Scenario: A several-group write is an ordinary tracked write with no lifecycle of its own
    When I publish an event of kind 30315 into the groups "darkroom" and "photographers"
    Then the write carries a store-issued receipt id
    And it was delivered to every host the scope named and to no other relay
