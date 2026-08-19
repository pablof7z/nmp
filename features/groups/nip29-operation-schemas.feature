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

  @nip29
  Scenario: A join request with an invite code drafts kind 9021 and a code tag
    When I compose a join request with invite code "dark-slide-42"
    Then the draft is kind 9021
    And the draft carries a code tag with value "dark-slide-42"
    And the draft carries no h tag

  @nip29
  Scenario: A join request with no invite code carries no code tag
    When I compose a join request with no invite code
    Then the draft is kind 9021
    And the draft carries no code tag
    And the draft carries no tag at all

  @nip29
  Scenario: A leave request drafts kind 9022 with no tags
    When I compose a leave request
    Then the draft is kind 9022
    And the draft carries no tag at all

  @nip29
  Scenario: Adding several users drafts one kind 9000 with every p tag
    When I compose adding users "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" and "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad"
    Then the draft is kind 9000
    And the draft carries a p tag naming "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"
    And the same draft carries a p tag naming "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad"

  @nip29
  Scenario: An empty or internally conflicting user batch is refused before publication
    When I compose adding no users or assign one user two different roles
    Then composition is refused
    And no event or receipt exists

  @nip29
  Scenario: Removing several users drafts one kind 9001 with every p tag
    When I compose removing users "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" and "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad"
    Then the draft is kind 9001
    And the draft carries a p tag naming "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"
    And the draft carries a p tag naming "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad"

  @nip29
  Scenario: Editing both metadata fields drafts kind 9002 with both tags
    When I compose editing the group metadata with name "Photographers" and about "film only, no spoilers"
    Then the draft is kind 9002
    And the draft carries a name tag with value "Photographers"
    And the draft carries an about tag with value "film only, no spoilers"

  @nip29
  Scenario: Editing only one metadata field leaves the other field's tag absent
    When I compose editing the group metadata with name "Photographers" and nothing else
    Then the draft carries a name tag with value "Photographers"
    And the draft carries no about tag
    And the draft carries no empty tag

  @nip29
  Scenario: Editing metadata drafts NIP-29's picture row and its marker rows
    When I compose editing the group metadata to a public closed workspace named "Workspace" pictured at "https://cdn.example/w.png"
    Then the draft is kind 9002
    And the draft carries a name tag with value "Workspace"
    And the draft carries a picture tag with value "https://cdn.example/w.png"
    And the draft carries a bare public tag
    And the draft carries a bare closed tag

  @nip29
  Scenario: Who may read and whether joins are honoured are independent two-valued choices
    Then each of NIP-29's four read/join combinations composes exactly its own two marker tags

  @nip29
  Scenario: An unstated marker emits no row, so an edit never clears what it did not mention
    When I compose editing the group metadata with name "Workspace" and nothing else
    Then the draft carries a name tag with value "Workspace"
    And the draft carries no marker tag at all

  @nip29
  Scenario: Creating a subgroup drafts NIP-29's parent row on the create itself
    When I compose creating a group under parent "darkroom"
    Then the draft is kind 9007
    And the draft carries a parent tag with value "darkroom"

  @nip29
  Scenario: A metadata edit states no parent, because no relay honours one there
    When I compose editing the group metadata to a public closed workspace named "Workspace" pictured at "https://cdn.example/w.png"
    Then the draft is kind 9002
    And the draft carries no parent tag

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
