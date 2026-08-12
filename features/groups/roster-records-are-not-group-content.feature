Feature: Asking for a group's roster through the wrong door says no
  An event published INTO a group carries a row naming which group it is in.
  The three records a relay signs ABOUT a group do not carry that row -- they
  identify themselves a different way entirely. They are not in the group;
  they are about it.

  So a read that scopes by "which group is this event in" and asks for those
  three records builds a request that no such record can ever match. It opens
  successfully, it returns nothing, and it goes on returning nothing forever.
  An app cannot tell that apart from a group whose relay has published no
  roster, which is a perfectly legitimate state -- so the failure is silent by
  construction, and it shipped.

  This feature is about NMP refusing that read instead of answering it with a
  permanent silence. A door that returns nothing forever is worse than one
  that says no.

  Traces to #1245 and to `crates/nmp-nip29/src/context.rs`.

  Background:
    Given a group hosted by one relay

  # nmp:id=GROUPS-RECORDSNOTCONTENT-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_read_selection_naming_the_relay_signed_records_is_refused_not_answered
  # nmp:evidence=rust:nmp::a_roster_read_through_the_content_door_is_refused_not_silently_empty
  # nmp:falsifier=Verified red-then-green in `nmp_nip29::group_demand_at` by removing the refusal and scoping the read by the group-context row anyway: `assertion left == right failed: the door must say no; a door that returns nothing forever is worse -- left: None, right: Some(Context(RecordsAreNotContextScoped { kinds: {39001, 39002} }))`. That is the shipped defect exactly: the read returns Ok, the subscription opens, no record can match, and the app sees an empty roster it cannot distinguish from a real one.
  @nip29
  Scenario Outline: A content read that asks for a relay-signed record is refused
    When the app asks to read <records> through the group's content door
    Then the read is refused
    And the refusal names <records>
    And no subscription is opened

    Examples:
      | records                          |
      | the group's metadata             |
      | the admin list                   |
      | the member list                  |
      | the admin list and the member list |

  # nmp:id=GROUPS-RECORDSNOTCONTENT-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_read_selection_naming_the_relay_signed_records_is_refused_not_answered
  # nmp:falsifier=Refuse only when the whole selection is relay-signed records, letting a mixed selection through with the unmatchable part silently dropped; the app would receive the chat messages it asked for and quietly nothing at all for the roster it asked for in the same breath, which is the original silent failure wearing a partial success.
  @nip29
  Scenario: Asking for chat and the roster together is still refused
    When the app asks to read chat messages and the member list in one go
    Then the read is refused
    And the refusal names the member list
    And the chat messages are not delivered separately as a consolation

  # nmp:id=GROUPS-RECORDSNOTCONTENT-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::the_records_refusal_names_both_tag_axes
  # nmp:falsifier=Refuse without saying why; the app author is told their read is invalid and is given nothing to act on -- and the natural next move, from the same evidence, is to conclude the roster is simply unreadable through NMP and go back to parsing raw tags, which is the outcome #1233 exists to end.
  @nip29
  Scenario: The refusal says enough to find the right door
    When the app asks to read the member list through the group's content door
    Then the refusal explains that these records identify themselves differently
    And it names which record was asked for

  # nmp:id=GROUPS-RECORDSNOTCONTENT-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::ordinary_group_content_still_reads_through_the_content_door
  # nmp:evidence=rust:nmp-nip29::a_read_branch_imposes_no_kind_catalogue_over_arbitrary_app_selections
  # nmp:falsifier=Widen the refusal from "these three records identify themselves by a different row" to a catalogue of what may live in a group; a chat message, a reaction, a moderation action or an application's own event type would start being refused or privileged by the group door, which is the fixed content catalogue that was removed once already (#838) coming back in a new spelling.
  @nip29
  Scenario Outline: Everything that really is in the group still reads normally
    When the app asks to read <content> through the group's content door
    Then the read succeeds
    And the request carries exactly what the app asked for
    And the group added nothing of its own except which group it is

    Examples:
      | content                              |
      | chat messages                        |
      | moderation actions the group defines |
      | events another spec defines          |
      | an event type nothing defines        |

  # nmp:id=GROUPS-RECORDSNOTCONTENT-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_empty_record_selection_is_refused_rather_than_observed
  # nmp:falsifier=Open the observation anyway when the app named no record; it would deliver a permanently empty state forever, indistinguishable from a group nothing has been published about -- the same silent failure this feature exists to close, reintroduced on the door that replaced it.
  @nip29
  Scenario: Watching no records at all is refused too
    When the app asks to watch the group's records but names none of them
    Then the request is refused
    And no subscription is opened
