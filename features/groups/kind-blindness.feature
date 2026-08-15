Feature: A group publishes every kind identically
  NIP-29 permits any kind to carry an h and live in a group. The group's whole
  contribution to an event is one h tag and one route; there is no kind it
  privileges, no kind it rejects, and no branch anywhere that reads the kind.
  Declaring a fixed content catalogue was a measured defect (#838); this
  invariant is what prevents it coming back in a new spelling.

  Traces to docs/internals/nip29/group-publication.md sections 4 and 7, and to
  scripts/check-nip29-kind-blindness.sh.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay

  # nmp:id=PROTOCOL-KINDBLINDNESS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::contextualize_takes_the_identical_path_for_every_kind_familiar_or_not
  # nmp:evidence=rust:nmp-nip29::a_read_branch_imposes_no_kind_catalogue_over_arbitrary_app_selections
  # nmp:evidence=script:repository::scripts/check-nip29-kind-blindness.sh
  # nmp:falsifier=making contextualize or group_demand_at inspect, filter, or special-case any one kind in the table (9021 NIP-29's own, 7/30315 other NIPs', 44815/20/1 unrecognised) makes contextualize_takes_the_identical_path_for_every_kind_familiar_or_not or a_read_branch_imposes_no_kind_catalogue_over_arbitrary_app_selections see a wrong tag set, a refusal, or a kind-dependent result for that one row while the others stay unaffected; adding any `Kind`/`.kind` reference to context.rs to implement such a branch independently makes check-nip29-kind-blindness.sh fail closed
  #
  # #1245 added the one selection the read door does refuse: NIP-29's three
  # relay-signed records, which do not carry an h row at all and so cannot
  # match an h-scoped request. That is a refusal about which row identifies
  # the event, not a catalogue of what may live in a group -- and it is
  # governed on its own terms in
  # features/groups/roster-records-are-not-group-content.feature, whose
  # GROUPS-RECORDSNOTCONTENT-004 asserts that everything genuinely in a group
  # still reads through the door untouched.
  @nip29
  Scenario Outline: A chat message, a reaction and a custom kind take one path
    When I publish an event of kind <kind> with content "<content>" through the group
    Then the published event is kind <kind>
    And the published event carries an h tag with value "photographers"
    And the published event was delivered to "wss://relay.groups.example"
    And no other relay received the published event
    And the group read the kind at no point in that publication

    Examples:
      | kind  | content              |
      | 9     | first light          |
      | 7     | +                    |
      | 31337 | exposure f/8, 1/125  |

  # nmp:id=PROTOCOL-KINDBLINDNESS-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::draft_kind_and_schema_survive_except_for_appended_h
  # nmp:evidence=rust:nmp-nip29::caller_supplied_own_h_is_refused_before_signing_or_routing
  # nmp:evidence=rust:nmp-nip29::caller_supplied_other_group_h_is_refused_the_same_way
  # nmp:evidence=rust:nmp::a_caller_supplied_context_never_reaches_the_door
  # nmp:falsifier=having contextualize drop, reorder, or mutate any caller-supplied field or tag other than appending the owned h row makes draft_kind_and_schema_survive_except_for_appended_h see a changed kind, content, created_at, or tag list; silently overwriting a caller-supplied h instead of refusing it makes caller_supplied_own_h_is_refused_before_signing_or_routing and caller_supplied_other_group_h_is_refused_the_same_way return Ok instead of CallerSuppliedContext, and makes a_caller_supplied_context_never_reaches_the_door see a receipt where none should exist
  @nip29
  Scenario: The group's only contribution to the event is the h tag
    Given an unsigned event of kind 31337 with content "exposure f/8, 1/125"
    And that event carries the tags "d"="portfolio" and "t"="landscape"
    And that event carries a created_at the app chose
    When I publish that event through the group
    Then the delivered event differs from the one I supplied only by an appended h tag
    And its kind, content and created_at survive unchanged
    And every tag I supplied survives unchanged and in the order I gave it

  # nmp:id=PROTOCOL-KINDBLINDNESS-003
  # nmp:status=built
  # nmp:evidence=script:repository::scripts/check-nip29-kind-blindness.sh
  # nmp:evidence=script:repository::scripts/check-nip29-surfaces.sh
  # nmp:falsifier=defining a kind-9-valued constant or a Kind::from(9) call anywhere in crates/nmp-nip29/src/operations.rs or discovery.rs makes check-nip29-kind-blindness.sh's owned-kind-literal enumeration see a value outside NIP-29's own 9000-9022/39000-39002 set and fail closed; a CHAT_KIND/compose_chat/GroupReply decoy name anywhere in crates/nmp-nip29/src makes check-nip29-kind-blindness.sh's decoy-name scan fail; adding a chat- or reaction-shaped composer function name to any of the four platform surfaces makes check-nip29-surfaces.sh's decoy scan fail. C7's independent kind:9 ownership (crates/nmp-nipc7) is scripts/check-nip29-ownership.sh's existing, unduplicated evidence (`bash`-invoked, not a separate CI lane) and is not re-cited here.
  @nip29
  Scenario: Kind 9 is not the group's kind
    When I publish an event of kind 9 with content "first light" through the group
    Then the group contributed no part of the kind 9 schema
    And the group exposes no composer for kind 9

  # nmp:id=PROTOCOL-KINDBLINDNESS-004
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::contextualize_takes_the_identical_path_for_every_kind_familiar_or_not
  # nmp:evidence=rust:nmp::an_unfamiliar_kind_is_published_not_questioned
  # nmp:falsifier=refusing, classifying, or special-casing kind 44815 (defined by no known NIP) anywhere on the unsigned contextualization path makes contextualize_takes_the_identical_path_for_every_kind_familiar_or_not see a refusal or a different tag set for that row alone; the same refusal at the supported-facade publish door makes an_unfamiliar_kind_is_published_not_questioned's expect() panic instead of returning a receipt stream
  @nip29
  Scenario: An unfamiliar kind is published, not questioned
    When I publish an event of kind 44815 with content "whatever this is" through the group
    Then the published event was delivered to "wss://relay.groups.example"
    And the publication was not refused for being an unrecognised kind
