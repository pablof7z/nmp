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

  # nmp:id=GROUPS-RECORDS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_subject_listed_only_by_the_second_host_still_appears_attributed_to_it
  # nmp:falsifier=Verified red-then-green in `nmp::nip29::records::union` by seeding the merge from only the first relay that answered (`.take(1)`): `assertion left == right failed: the union must name every subject either relay listed, including the one only the SECOND relay listed -- left: {49f1..., e99a...}, right: {39b5..., 49f1..., e99a...}`. A subject the second relay alone lists disappears from the roster entirely, and the roster still looks complete.
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

  # nmp:id=GROUPS-RECORDS-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::metadata_is_one_hosts_whole_record_never_a_field_wise_merge
  # nmp:falsifier=Verified red-then-green in `nmp::nip29::records::project` by filling any field the winning record left empty from another relay's record: `assertion left == right failed: the winning relay signed no about row, so there is no about row -- left: Some("an about row only the OLDER record carries"), right: None`. The app then renders a title and a description that no single relay ever signed together, and nothing on screen distinguishes that from a real record.
  @nip29
  Scenario: The group's description is one relay's whole record, never a blend of two
    Given the first relay published a newer record giving the group a name and no description
    And the second relay published an older record giving it a different name and a description
    When the app watches the group's metadata
    Then it is shown the newer relay's name
    And it is shown no description at all
    And it can see which relay signed what it is showing
    And the older relay's whole record, description included, is still reachable beside it

  # nmp:id=GROUPS-RECORDS-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_admin_with_no_role_is_not_reported_as_a_member
  # nmp:evidence=rust:nmp-nip29::an_untagged_subject_has_no_role_and_is_never_defaulted_to_member
  # nmp:falsifier=Verified red-then-green in `nmp_nip29::records::listed_record_at` by defaulting a missing role to `"member"`: `assertion left == right failed: a relay that wrote no role must not be reported as having written one -- left: Some("member"), right: None`. This is the shipped defect in one of the hand-rolled readers being replaced: an admin the relay listed with no role beside them is silently recorded as an ordinary member, and the app's own moderation checks then fail for a real moderator.
  @nip29
  Scenario: An admin with no role written beside them is not turned into a member
    Given the relay's admin list names Dana as a moderator and names Eli with no role
    And the relay's member list names Finn
    When the app watches the group's admins and members
    Then Dana is shown as an admin with the role the relay wrote
    And Eli is shown as an admin with no role
    And Eli does not appear on the member list
    And Finn is shown as a member with no role

  # nmp:id=GROUPS-RECORDS-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::differs_answers_the_dig_in_question_both_ways
  # nmp:falsifier=Report disagreement whenever more than one relay answered, rather than comparing what they said; two relays publishing identical member lists would then be flagged as disagreeing, and an app offering a "the relays differ" affordance would show it permanently, for every multi-relay group, meaning nothing.
  @nip29
  Scenario: The app can tell whether the relays actually disagree
    Given both relays publish the same member list
    When the app watches the group's member list
    Then it is told the relays do not disagree
    And the one member appears once, marked as listed by both

  # nmp:id=GROUPS-RECORDS-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_group_scoped_door_delivers_one_snapshot_from_the_first_delivery
  # nmp:falsifier=Deliver nothing until a record arrives; a room screen opening on a group whose relay has published nothing yet has no value at all to render, cannot distinguish "still loading" from "nothing there", and has to invent a placeholder of its own -- which is the state each application currently invents differently.
  @nip29
  Scenario: Opening a room shows something before any record has arrived
    Given the relay has published nothing at all about this group
    When the app watches that one group by its id
    Then it is immediately handed that group's state
    And that state carries no metadata and no members
    And it reports that the records are still being acquired
    And when the relay later publishes the group's name, the app is handed it

  # nmp:id=GROUPS-RECORDS-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_composed_predicate_delivers_one_snapshot_per_matching_group
  # nmp:evidence=rust:nmp::a_literal_id_set_lowers_to_the_d_values_themselves
  # nmp:falsifier=Remove the literal-id leaf so a known set of groups can only be watched by naming somebody who is listed in all of them; an app watching the rooms a user saved would have to invent a subject that is a member of every one of them, which is not always true and is not what the app meant.
  @nip29
  Scenario: One watch covers the groups I belong to and the ones I saved
    Given the relay lists me as a member of one group
    And I saved a second group that does not list me
    When the app watches groups matching either condition
    Then it is handed one state per matching group
    And both groups are among them

  # nmp:id=GROUPS-RECORDS-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_records_listing_never_lets_one_hosts_member_evidence_answer_for_anothers_group
  # nmp:falsifier=Verified red-then-green when this test first landed (#1033) by leaving the nested per-host lookup on the grammar's default cache mode: one relay's member-list record then answered the other relay's structurally identical lookup out of the shared local store, and a group nothing at the second relay supported was reported as existing there. Widened here to assert on what the app is handed, so the leak is caught as a wrong roster rather than only as a wrong row.
  @nip29
  Scenario: One relay's evidence never answers for another relay's group of the same name
    Given both relays host a group with the same name
    And only the first relay's member list names me
    When the app watches the groups whose member list names me
    Then it is shown the first relay's group
    And nothing is attributed to the second relay
    And the metadata it is shown was signed by the first relay
