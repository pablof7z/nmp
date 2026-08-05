Feature: A NIP-29 receipt proves the request was accepted, never that the group changed
  NIP-29 moderation is two-phase by construction. Phase one is a client-signed
  900x request; phase two is the relay's own signed 39000/39001/39002, which is
  what every other participant actually reads. A `WriteStatus::Acked` proves a
  host took the request event. It does not prove the host applied it, and the
  relay is under no obligation to publish its records on any schedule.

  So NMP models both, and keeps them apart. The receipt is the ACK. The
  relay-signed records observation (#1246) is the truth. `GroupSnapshot::
  member_listing`/`admin_listing` is where an app asks the question a receipt
  cannot answer, and it answers in the three ways NIP-29 admits -- including
  the one no receipt can express: settled absence.

  What is deliberately NOT here is a wait, a poll, a backoff or a deadline.
  The consumer audit behind #1234 found 8 hand-rolled poll loops with 4
  different backoff policies in one app, several of them REPUBLISHING the same
  moderation event each round -- not because they believed the send failed,
  but because they had no other lever while waiting. The lever is settlement:
  `GroupAvailability::Ready` means every source in every host's plan has
  reconciled and is live, which is a fact about knowledge rather than a guess
  about elapsed time. Nothing in this feature sleeps.

  The two-phase shape is true of every named operation, not of a subset:
  9007 and 9002 are answered by a 39000, 9000/9001 by a 39001/39002, and
  9021/9022 by a 39002. Only an ordinary content write is one-phase, and
  NIP-29 owns no content schema for it to be one of.

  # nmp:id=PROTOCOL-RECEIPTISNOTAROSTERCHANGE-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::absence_is_claimed_only_once_every_host_has_settled
  # nmp:evidence=rust:nmp-ffi::absence_crosses_the_boundary_only_once_every_host_has_settled
  # nmp:evidence=swift:NMP::testAbsenceIsClaimedOnlyOnceEveryHostHasSettled
  # nmp:evidence=kotlin:NMPKotlin::absenceIsClaimedOnlyOnceEveryHostHasSettled
  # nmp:falsifier=Deciding the negative on anything other than whole-scope settlement -- a timer, a first delivery, one host's own readiness, or an unconditional "no entry means absent" -- makes absence_is_claimed_only_once_every_host_has_settled report Absent for at least one of Acquiring/CachedOnly/SourceUnavailable over the identical empty union, and the FFI/Swift/Kotlin mirrors fail the same assertion at their own boundary.
  @nip29
  Scenario: Absence is claimed only once every host has settled
    Given a group whose member list is observed on two hosts
    When one host has not finished reconciling its records
    Then asking whether a subject is listed answers that it is unestablished
    And that answer is not a claim of absence
    And nothing waited, slept or retried to produce it
    When every host in the scope has reconciled and none names the subject
    Then asking whether that subject is listed answers that it is absent

  # nmp:id=PROTOCOL-RECEIPTISNOTAROSTERCHANGE-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_record_the_observation_never_asked_for_is_never_reported_absent
  # nmp:evidence=rust:nmp-ffi::a_record_the_observation_never_asked_for_never_crosses_as_absent
  # nmp:evidence=swift:NMP::testARecordTheObservationNeverAskedForIsNeverAbsent
  # nmp:evidence=kotlin:NMPKotlin::aRecordTheObservationNeverAskedForIsNeverAbsent
  # nmp:falsifier=Dropping `selected` from the snapshot, or deciding the negative on availability alone, makes a_record_the_observation_never_asked_for_is_never_reported_absent report Absent for a metadata-only observation that never requested a member list -- an absence claimed about a record nobody asked for. Failing to project `selected` across the FFI boundary reproduces the same lie one layer out, which a_record_the_observation_never_asked_for_never_crosses_as_absent catches.
  @nip29
  Scenario: A record the observation never asked for is never reported absent
    Given a group whose metadata alone is observed
    When every host has reconciled that metadata
    Then asking whether a subject is in the member list answers unestablished
    And asking whether that subject is in the admin list answers unestablished
    And neither answer is a claim of absence

  # nmp:id=PROTOCOL-RECEIPTISNOTAROSTERCHANGE-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::inclusion_is_evidence_before_anything_has_settled
  # nmp:evidence=rust:nmp::two_hosts_disagreeing_about_a_role_are_both_reported
  # nmp:evidence=rust:nmp-ffi::inclusion_crosses_with_its_role_and_hosts_before_anything_settles
  # nmp:evidence=swift:NMP::testInclusionIsEvidenceBeforeAnythingSettles
  # nmp:evidence=kotlin:NMPKotlin::inclusionIsEvidenceBeforeAnythingSettles
  # nmp:falsifier=Gating inclusion on settlement -- requiring Ready before reporting a subject a relay has already signed a record naming -- makes inclusion_is_evidence_before_anything_has_settled see Unestablished instead of Named while a second host is still acquiring. Collapsing two hosts' differing roles into one winner makes two_hosts_disagreeing_about_a_role_are_both_reported see one entry where two are required.
  @nip29
  Scenario: Inclusion is evidence at any availability, with the role each host wrote
    Given a group whose member list is observed on two hosts
    When one host's own signed record names a subject as a moderator
    And the other host has not finished reconciling
    Then asking whether that subject is listed answers that it is named
    And the answer carries the host that named it and the role it wrote
    When the two hosts wrote different roles for that subject
    Then both are reported, attributed to the host that wrote each
    And no single role is invented as the winner

  # nmp:id=PROTOCOL-RECEIPTISNOTAROSTERCHANGE-004
  # nmp:status=built
  # nmp:evidence=rust:nmp-parity::direct_and_ffi_listings_agree_on_every_settlement_and_selection_case
  # nmp:evidence=rust:nmp-ffi::a_malformed_subject_is_refused_rather_than_read_as_absent
  # nmp:evidence=swift:NMP::testAMalformedSubjectThrowsRatherThanReadingAsAbsent
  # nmp:evidence=kotlin:NMPKotlin::aMalformedSubjectThrowsRatherThanReadingAsAbsent
  # nmp:falsifier=Letting the FFI boundary form its own opinion about the negative makes exactly one of direct_and_ffi_listings_agree_on_every_settlement_and_selection_case's 36 availability-by-selection-by-union combinations diverge from the direct answer. Treating an unparseable subject as a no-match rather than a typed refusal makes a_malformed_subject_is_refused_rather_than_read_as_absent see Absent -- a settled claim about a pubkey that was never understood.
  @nip29
  Scenario: The same rule answers on every surface, and a subject it cannot read is refused
    When I ask the same listing question through Rust and through the FFI boundary
    Then both answer identically for every settlement and selection combination
    And neither boundary carries its own opinion about what absence means
    When I ask about a subject that is not a well-formed public key
    Then the question is refused with a typed error
    And it is never answered as a settled absence
