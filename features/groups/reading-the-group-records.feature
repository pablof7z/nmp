Feature: An app can read who is in a group, and what it is called
  NIP-29 defines three records a relay signs about a group: its metadata, its
  admin list and its member list. Until #1233 an app could only ask NMP
  whether one of those lists names a particular person. It could not ask who
  they name. So both real applications walked the raw tags themselves, four
  separate times, and the four readings disagreed with each other: one dropped
  the role beside an admin that another kept, and one recorded an admin with
  no role written beside them as an ordinary member.

  These scenarios are about what an app observes when it watches those records
  now: what it is handed, what two relays disagreeing looks like, and what NMP
  refuses to invent on its behalf.

  A group can live on more than one relay, and two relays hosting the same
  group name are two independent groups. So every claim below has to say what
  happens when the two disagree, not only when they agree.

  Traces to #1233 and to `crates/nmp/tests/group_records_reader.rs`.

  Background:
    Given a group hosted by two relays

  @nip29
  Scenario: Someone only the second relay lists still appears, and it is clear who said so
    Given the first relay lists Ana and Ben as members
    And the second relay lists Ben and Cleo as members
    When the app watches the group's member list
    Then it is shown Ana, Ben and Cleo
    And Ana is marked as listed by the first relay only
    And Cleo is marked as listed by the second relay only
    And Ben is marked as listed by both
    And the app can also see exactly the two names the first relay listed, on their own

  @nip29
  Scenario: The group's description is one relay's whole record, never a blend of two
    Given the first relay published a newer record giving the group a name and no description
    And the second relay published an older record giving it a different name and a description
    When the app watches the group's metadata
    Then it is shown the newer relay's name
    And it is shown no description at all
    And it can see which relay signed what it is showing
    And the older relay's whole record, description included, is still reachable beside it

  @nip29
  Scenario: An admin with no role written beside them is not turned into a member
    Given the relay's admin list names Dana as a moderator and names Eli with no role
    And the relay's member list names Finn
    When the app watches the group's admins and members
    Then Dana is shown as an admin with the role the relay wrote
    And Eli is shown as an admin with no role
    And Eli does not appear on the member list
    And Finn is shown as a member with no role

  @nip29
  Scenario: The app can tell whether the relays actually disagree
    Given both relays publish the same member list
    When the app watches the group's member list
    Then it is told the relays do not disagree
    And the one member appears once, marked as listed by both

  @nip29
  Scenario: Opening a room shows something before any record has arrived
    Given the relay has published nothing at all about this group
    When the app watches that one group by its id
    Then it is immediately handed that group's state
    And that state carries no metadata and no members
    And it reports that the records are still being acquired
    And when the relay later publishes the group's name, the app is handed it

  @nip29
  Scenario: One watch covers the groups I belong to and the ones I saved
    Given the relay lists me as a member of one group
    And I saved a second group that does not list me
    When the app watches groups matching either condition
    Then it is handed one state per matching group
    And both groups are among them

  @nip29
  Scenario: One relay's evidence never answers for another relay's group of the same name
    Given both relays host a group with the same name
    And only the first relay's member list names me
    When the app watches the groups whose member list names me
    Then it is shown the first relay's group
    And nothing is attributed to the second relay
    And the metadata it is shown was signed by the first relay
