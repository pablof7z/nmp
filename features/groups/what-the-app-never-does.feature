Feature: The app never names the host, the route, or the context tag
  The boundary stated as observable behaviour rather than as a convention. If
  any of these becomes possible again, a group stops being the only door and
  the app is back to spelling routing values it has no business knowing.

  Traces to docs/internals/nip29/group-publication.md section 8 (the boundary
  table) and to docs/internals/routing/auto-and-explicit.md section 5.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay
    And a relay "wss://bystander.example" exists that nothing references

  # nmp:id=PROTOCOL-WHATTHEAPPNEVERDOES-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox
  # nmp:evidence=rust:nmp::a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope
  # nmp:falsifier=having any group operation contact, fall back to, or additionally route through a relay outside the scope's own retained host set (the author's discovered outbox, or a bystander relay nothing references) makes a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox see contact where the test proves none, and makes a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope see relays_named_by(...) diverge from exactly the scope's hosts
  @nip29 @must-never
  Scenario: Every relay a group write touches traces back to the group's own identity
    When I publish an event of kind 9 with content "first light" through the group
    And I publish a join request through the group
    And I remove user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then every contacted relay is "wss://relay.groups.example"
    And the app supplied no relay anywhere in that run
    And relay "wss://bystander.example" received no connection at all

  # nmp:id=PROTOCOL-WHATTHEAPPNEVERDOES-002
  # nmp:status=built
  # nmp:evidence=script:repository::scripts/check-nip29-operation-catalogue.sh
  # nmp:falsifier=adding a relay, route or host parameter to any of the nine named operations, or to publish/publishSigned/read, on any of the four surfaces (the Rust facade, the Rust FFI, Swift, Kotlin) makes check-nip29-operation-catalogue.sh's per-surface signature-shape checks fail with "takes a per-call relay, route or host parameter" or "takes a raw kind, tag, relay or route parameter"
  @nip29 @must-never
  Scenario: There is no way to name a relay on a group write
    When I inspect the group's write surface
    Then no group write operation accepts a relay
    And no group write operation accepts a routing value
    And a group write cannot be redirected to a relay other than its host

  # nmp:id=PROTOCOL-WHATTHEAPPNEVERDOES-003
  # nmp:status=built
  # nmp:evidence=script:repository::scripts/check-nip29-operation-catalogue.sh
  # nmp:evidence=rust:nmp-nip29::caller_supplied_own_h_is_refused_before_signing_or_routing
  # nmp:evidence=rust:nmp-nip29::caller_supplied_other_group_h_is_refused_the_same_way
  # nmp:evidence=rust:nmp-nip29::draft_kind_and_schema_survive_except_for_appended_h
  # nmp:evidence=rust:nmp-grammar::the_ordinary_builder_accepts_an_h_shaped_tag_with_no_validation
  # nmp:falsifier=adding an h/context parameter to any group write operation on any of the four surfaces makes check-nip29-operation-catalogue.sh fail; silently overwriting instead of refusing a caller-supplied h makes caller_supplied_own_h_is_refused_before_signing_or_routing and caller_supplied_other_group_h_is_refused_the_same_way return Ok; deriving the appended h from anything other than the group id given at construction makes draft_kind_and_schema_survive_except_for_appended_h see the wrong value. This claim is about the semantic NIP-29 door only, not impossibility throughout the repository -- the_ordinary_builder_accepts_an_h_shaped_tag_with_no_validation is the positive control proving the ordinary EventBuilder escape (nmp-grammar's `tag(Tag)`, #1034's one intentional exact/raw escape) stays exactly as permissive outside a Group as it always was; that test regressing to a refusal would be evidence the door had been widened into the general builder, not narrowed as claimed
  @nip29 @must-never
  Scenario: There is no way to set the h tag through the group
    When I inspect the group's write surface
    Then no group write operation accepts an h value
    And an event that arrives carrying its own h is refused
    And the group id given at construction is the only source of the h tag

  # nmp:id=PROTOCOL-WHATTHEAPPNEVERDOES-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox
  # nmp:falsifier=a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox first proves the author's real outbox is reachable (an ordinary Auto-routed publish resolves to it) so its absence from a group write's contacted relays cannot be explained by the outbox being unreachable; having the group write additionally contact, or fall back to, that same proven-real outbox makes its relays_named_by(...) assertion see it
  @nip29 @must-never
  Scenario: A group write never enters the author's outbox lane
    When I publish an event of kind 9 with content "first light" through the group
    Then diagnostics show the write on an explicit route
    And diagnostics show no outbox resolution for that write
    And no relay list of mine was read for that write

  # nmp:id=PROTOCOL-WHATTHEAPPNEVERDOES-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_read_never_widens_beyond_its_pinned_host_to_a_discovered_author_outbox
  # nmp:falsifier=a_group_read_never_widens_beyond_its_pinned_host_to_a_discovered_author_outbox first proves the author's outbox is a real, resolvable routing fact for the active identity (FixtureRoutingFacts::with_outbound_routes), so its absence from the group read cannot be explained by the fact not existing; having the group subscription's wire sessions or acquisition evidence include that outbox relay, or any relay beyond the group's pinned host, makes its session-set or evidence-source assertions see it
  @nip29 @must-never
  Scenario: A group read is pinned by the group, never widened by what the engine learns
    Given a filter selecting kind 9
    And the engine later learns of relay "wss://gossip.example" for this group's members
    When I observe a live query built from the group's demand for that filter
    Then the request is pinned to "wss://relay.groups.example"
    And relay "wss://gossip.example" received no connection at all
    And the pinned set was never widened
