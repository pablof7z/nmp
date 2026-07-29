Feature: NIP-29's own operations are named, not hand-assembled
  Join, leave and moderation are kinds NIP-29 itself defines, so the group
  offers them as named operations. Without that, every app looks up the kind
  numbers and the tag schema itself, and a subtly wrong tag comes back as a
  relay rejection that reads like a permissions or routing problem rather than
  a malformed event. The knowledge lives in one place or it is reimplemented,
  differently, in every consumer.

  These operations are ordinary group publications: same h, added before
  signing, same explicit route to the host, same receipt.

  Traces to docs/internals/nip29/group-publication.md section 7.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay

  @nip29
  Scenario: A join request with an invite code
    When I publish a join request with invite code "dark-slide-42" through the group
    Then the published event is kind 9021
    And the published event carries an h tag with value "photographers"
    And the published event carries a code tag with value "dark-slide-42"
    And the published event was delivered to "wss://relay.groups.example"
    And no other relay received the published event

  @nip29
  Scenario: A join request with no invite code carries no code tag
    When I publish a join request with no invite code through the group
    Then the published event is kind 9021
    And the published event carries an h tag with value "photographers"
    And the published event carries no code tag
    And the published event carries no empty tag

  @nip29
  Scenario: A leave request
    When I publish a leave request through the group
    Then the published event is kind 9022
    And the published event carries an h tag with value "photographers"
    And the published event was delivered to "wss://relay.groups.example"

  @nip29
  Scenario: Adding a user
    When I add user "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" to the group
    Then the published event is kind 9000
    And the published event carries an h tag with value "photographers"
    And the published event carries a p tag naming "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"
    And the published event was delivered to "wss://relay.groups.example"

  @nip29
  Scenario: Adding a user with a role
    When I add user "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" to the group with role "moderator"
    Then the published event is kind 9000
    And the published event carries a p tag naming "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" with role "moderator"

  @nip29
  Scenario: Removing a user
    When I remove user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then the published event is kind 9001
    And the published event carries an h tag with value "photographers"
    And the published event carries a p tag naming "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad"
    And the published event was delivered to "wss://relay.groups.example"

  @nip29
  Scenario: Editing the group metadata
    When I edit the group metadata with name "Photographers" and about "film only, no spoilers"
    Then the published event is kind 9002
    And the published event carries an h tag with value "photographers"
    And the published event carries a name tag with value "Photographers"
    And the published event carries an about tag with value "film only, no spoilers"

  @nip29
  Scenario: Editing only one metadata field leaves the others untouched
    When I edit the group metadata with name "Photographers" and nothing else
    Then the published event is kind 9002
    And the published event carries a name tag with value "Photographers"
    And the published event carries no about tag
    And the published event carries no empty tag

  @nip29
  Scenario: A moderation action the host refuses surfaces truthfully
    Given I am not an admin of "photographers"
    And relay "wss://relay.groups.example" rejects kind 9001 with "restricted: you are not an admin of this group"
    When I remove user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then the receipt reports the event rejected by "wss://relay.groups.example"
    And the receipt carries the message "restricted: you are not an admin of this group"
    And the removal is never reported as accepted
    And no other relay was tried
    And the operation was not retried anywhere else

  @nip29
  Scenario: A refused moderation action is a relay rejection, not a guess
    Given I am not an admin of "photographers"
    And relay "wss://relay.groups.example" rejects kind 9001 with "restricted: you are not an admin of this group"
    When I remove user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then the failure is reported as a rejection by the host
    And the failure is not reported as a routing failure
    And NMP made no claim of its own about my permissions in the group

  @nip29
  Scenario Outline: Every operation takes the same path as an ordinary publication
    When I invoke the group operation <operation>
    Then the published event carries an h tag with value "photographers"
    And the h tag was present in the bytes that were signed
    And the write's routing is explicit over exactly "wss://relay.groups.example"
    And relay "wss://alice-write.example" received no event
    And a receipt exists for it addressed by its event id

    Examples:
      | operation     |
      | join request  |
      | leave request |
      | add user      |
      | remove user   |
      | edit metadata |

  @nip29
  Scenario: The app names no kind number and no tag name to invoke an operation
    When I remove user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then I named no kind number on that call
    And I named no tag name on that call
    And I named no relay on that call

  @nip29
  Scenario: The group offers no composer for a kind NIP-29 does not define
    When I inspect the group's operation surface
    Then it offers operations only for kinds NIP-29 itself defines
    And it offers no chat composer
    And it offers no reaction composer
    And an app that wants either builds the event itself and publishes it through the group
