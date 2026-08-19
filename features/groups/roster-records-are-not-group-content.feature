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

  @nip29
  Scenario: Asking for chat and the roster together is still refused
    When the app asks to read chat messages and the member list in one go
    Then the read is refused
    And the refusal names the member list
    And the chat messages are not delivered separately as a consolation

  @nip29
  Scenario: The refusal says enough to find the right door
    When the app asks to read the member list through the group's content door
    Then the refusal explains that these records identify themselves differently
    And it names which record was asked for

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

  @nip29
  Scenario: Watching no records at all is refused too
    When the app asks to watch the group's records but names none of them
    Then the request is refused
    And no subscription is opened
