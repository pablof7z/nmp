Feature: A replaceable edit says which version it replaces, and is checked against the one actually stored
  A replaceable event has one winner at a time, and editing it means replacing
  the whole value. Two things have to be true for that to be safe, and they
  are the same two things every time: the edit has to be derived from the
  version that is really there, and its timestamp has to be greater than that
  version's -- or relays and peers keep serving the loser and the edit
  silently does nothing.

  So an edit travels with a precondition naming the version it believes it is
  replacing, and that precondition is checked inside the acceptance
  transaction, against the row acceptance is about to write. Not before, not
  optimistically, and not against whatever the app read a moment ago. If the
  winner moved in between -- another device, another tab, a sync that landed
  -- the write is refused with a typed conflict that names what was expected
  and what is actually there. It is never silently applied on top.

  The timestamp is decided in that same transaction, against that same row.
  This is the part that gets strictly better rather than merely rearranged.
  An app cannot compute a correct timestamp, because the only thing it can
  compute against is the copy it is holding, and the copy it is holding may
  already be behind. Its clock may be behind too. Deciding the stamp inside
  the transaction removes both problems at once: the row the stamp is
  computed against is the row the precondition is holding, so a stale base
  cannot produce a stale stamp -- a stale base does not get that far.

  An edit against a version somebody else authored needs no special error and
  gets none. The precondition is checked at the editing identity's own
  coordinate, and another author's event is never the winner there, so a
  foreign base is simply unsatisfiable and reports through the same conflict
  door as every other stale one. One conflict door, whatever the staleness's
  cause.

  Background:
    Given I am logged in as the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And my relay list names "wss://hub.example" as my write relay
    And my contact list "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" created at "2026-07-29T12:00:00Z" is the stored winner

  # ---- complete before custody ------------------------------------------

  # A capability operation is accepted only after that capability has
  # produced the whole unsigned replacement. NMP does not accept a promise to
  # fill in content, tags, or an event id later. Failure to produce the whole
  # event is therefore a refusal before custody, not a parked write.

  # nmp:id=WRITES-REPLACEABLE-EDIT-010
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip02::alice_then_bob_keep_two_receipts_and_one_complete_pending_event
  # nmp:falsifier=Accept the operation without committing its complete pending row; the public Engine capstone cannot observe the acceptance event id as the current Pending live-query value before returning custody.
  Scenario: An accepted capability operation already has one complete replacement event
    Given I am disconnected from every relay
    When a configured capability adds Alice to my contact list
    Then the write is accepted through the ordinary write-intent lifecycle
    And its ordinary receipt names the complete replacement event
    And the complete signature-pending replacement is the current live-query value
    And no accepted state is waiting for content, tags, or an event id

  # nmp:id=WRITES-REPLACEABLE-EDIT-011
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip02::invalidated_registration_and_materializer_refusal_leave_no_custody
  # nmp:falsifier=Accept an operation whose compiled capability was not supplied at construction or retain anything after synchronous materializer refusal; the queue is no longer empty and the signed source is no longer the sole canonical row.
  Scenario: An unavailable capability refuses the operation before custody
    Given the capability required by the operation is not configured
    When I try to add Alice to my contact list through that capability
    Then publishing is refused with a typed configuration error
    And NMP retains no receipt, write intent, optimistic row, signing work, route, delivery work, or correlation

  # nmp:id=WRITES-REPLACEABLE-EDIT-024
  # nmp:status=built
  # nmp:evidence=rust:nmp::repeated_materializations_do_not_change_the_process_thread_count
  # nmp:evidence=script:repository::scripts/check-no-detached-materializer.sh
  # nmp:falsifier=Restore one OS thread per materialization; the process thread census grows after repeated follow and successor work.
  @acceptance
  Scenario: A trusted capability edit runs without starting another thread
    Given a compiled contact-list capability is supplied when NMP starts
    When I follow Alice while offline
    And a newer remote contact list later arrives
    Then the complete replacement is visible immediately
    And repeated initial and successor edits do not change the process thread count

  # nmp:id=WRITES-REPLACEABLE-EDIT-025
  # nmp:status=built
  # nmp:evidence=rust:nmp::missing_compiled_capability_refuses_open_and_leaves_the_store_unchanged
  # nmp:falsifier=Open an engine whose retained follow work lacks its compiled program/format; construction succeeds or the store is mutated.
  @acceptance
  Scenario: Reopening retained work without its compiled capability fails at the door
    Given I accepted a follow while offline
    And I close NMP
    When I reopen the same store without that compiled capability
    Then construction is refused
    And the store is unchanged

  # The encrypted content is opaque to an operation that owns only a public
  # tag. Its presence does not turn that operation into a crypto operation.

  # nmp:id=WRITES-REPLACEABLE-EDIT-012
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip02::alice_then_bob_keep_two_receipts_and_one_complete_pending_event
  # nmp:evidence=rust:nmp-nip02::semantic_operations_compose_in_order_and_preserve_unowned_fields
  # nmp:falsifier=Rewrite or decrypt content while applying the public follow operation; the exact opaque source content no longer survives both materialization and durable restart.
  Scenario: A public tag-only edit preserves opaque encrypted content without crypto
    Given my stored contact list contains opaque encrypted content
    And no decryption capability is available
    When a configured capability adds Alice as a public contact tag
    Then the write is accepted
    And the replacement preserves the encrypted content byte for byte
    And NMP does not request decryption or encryption

  # This is the contrast with the preceding scenario: crypto is required by
  # what the operation asks to change, not merely by encrypted bytes being
  # present elsewhere in the event.

  # nmp:id=WRITES-REPLACEABLE-EDIT-013
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1382
  Scenario: An encrypted-content edit without its required crypto refuses before custody
    Given my stored contact list contains opaque encrypted content
    And the requested operation must decrypt and rewrite that content
    And the required crypto capability is unavailable
    When I try to publish that operation
    Then publishing is refused with a typed crypto-capability error
    And NMP retains no receipt, write intent, optimistic row, signing work, route, delivery work, or correlation

  # nmp:id=WRITES-REPLACEABLE-EDIT-014
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip02::alice_then_bob_keep_two_receipts_and_one_complete_pending_event
  # nmp:evidence=rust:nmp-store::body_complete_receipt_keeps_accepted_id_while_current_advances_across_reopen
  # nmp:falsifier=Create one receipt per shared event or rewrite Alice's acceptance id when Bob becomes current; the capstone loses two stable receipt identities or the store reopen proof observes the wrong accepted-to-current pair.
  Scenario: Several offline operations keep their receipts while sharing one complete current event
    Given I am disconnected from every relay
    When a configured capability adds Alice to my contact list
    And the configured capability then adds Bob to my contact list
    Then both operations have distinct ordinary receipts
    And one complete current signature-pending event contains Alice and Bob
    And both receipts name that current event without creating another receipt lifecycle

  # nmp:id=WRITES-REPLACEABLE-EDIT-015
  # nmp:status=built
  # nmp:evidence=rust:nmp::newer_relay_sources_install_complete_successors_without_new_receipts
  # nmp:evidence=rust:nmp-store::qualified_source_and_complete_successor_survive_redb_reopen
  # nmp:falsifier=Install the newer relay source as the visible winner before replaying Alice and Bob; the live query exposes the raw source or loses a local operation instead of moving directly between complete local generations.
  Scenario: A newer relay version is combined with every active local operation
    Given Alice and Bob were added to my contact list while offline
    And a relay later supplies a newer contact list containing Carol
    When NMP applies the configured contact-list capability to that newer version
    Then one complete current replacement contains Alice, Bob, and Carol
    And its timestamp is exactly one second after the newer relay version
    And the live query moves directly from the prior complete replacement to the successor
    And the raw relay version is never the effective live-query value
    And the original operation receipts now name the successor as current

  # nmp:id=WRITES-REPLACEABLE-EDIT-016
  # nmp:status=built
  # nmp:evidence=rust:nmp::newer_relay_sources_install_complete_successors_without_new_receipts
  # nmp:evidence=rust:nmp-store::semantic_source_and_effective_successor_are_one_crash_atomic_transition
  # nmp:falsifier=Commit the qualified relay source, successor row, or receipt updates separately; process death recovers a mixed source/effective generation or a partially advanced receipt set.
  Scenario: Source and successor recover as one durable state
    Given active contact-list operations have one complete current replacement over B0
    And a relay supplies newer source B5
    When NMP crashes while replacing the current event with the successor over B5
    Then reopen recovers either the complete B0 generation or the complete B5 generation
    And it never recovers raw B5 as the effective value
    And every original receipt names the same recovered current generation

  # nmp:id=WRITES-REPLACEABLE-EDIT-017
  # nmp:status=built
  # nmp:evidence=rust:nmp::relay_source_successors_resume_current_delivery_and_remain_continuing_after_restart
  # nmp:evidence=rust:nmp::stale_predecessor_delivery_callbacks_cannot_touch_the_current_successor
  # nmp:evidence=rust:nmp-store::semantic_source_and_effective_successor_are_one_crash_atomic_transition
  # nmp:falsifier=Forget the unsigned current generation's durable routes during restart or dispatch a predecessor callback by receipt instead of exact event id; E2 does not resume every destination or stale E1 work advances current state.
  Scenario: A successor retires predecessor work and republishes to every destination
    Given relay 1 received current generation E1
    And relay 2 later supplies a newer source version
    When NMP creates successor generation E2
    Then E1 signer, handoff, acknowledgement, timeout, authentication, and retry completions cannot advance E2 or put E1 back on the wire
    And E2 has fresh delivery work for relay 1 and relay 2
    And after restart only E2 resumes active delivery
    And E1 delivery evidence remains historical evidence naming E1

  # nmp:id=WRITES-REPLACEABLE-EDIT-018
  # nmp:status=built
  # nmp:evidence=rust:nmp::shared_second_generation_is_once_per_relay_and_replays_without_settling
  # nmp:falsifier=Suppress the physical owner's E2 Signing(Signed) receipt fact while leaving E2 delivery intact; the original contributing receipt no longer observes the shared generation signature.
  Scenario: Shared operation receipts observe one physical generation delivery
    Given Alice and Bob have distinct operation receipts sharing current generation E2
    And their destination plans overlap
    When E2 is signed and delivered
    Then exactly one signer request and one physical publication per relay occur for E2
    And both receipts expose signing and relay evidence naming E2

  # nmp:id=WRITES-REPLACEABLE-EDIT-019
  # nmp:status=built
  # nmp:evidence=rust:nmp::relay_source_successors_resume_current_delivery_and_remain_continuing_after_restart
  # nmp:falsifier=Treat a relay replay of terminal E2 as a new semantic source after restart; it supersedes signed E3 before E3 reaches every existing destination.
  Scenario: Destination completion does not close a continuing semantic operation
    Given every destination for the current semantic generation is terminal
    When its deliberately continuing source policy remains active
    Then each operation receipt remains open with event-qualified terminal relay evidence
    And a later qualified source may still create one successor generation
    And no terminal receipt is resurrected

  # nmp:id=WRITES-REPLACEABLE-EDIT-021
  # nmp:status=built
  # nmp:evidence=rust:nmp::finite_sources_are_exact_requests_restart_unfinished_and_close_with_destinations
  # nmp:falsifier=Stop consuming the retired hidden source observation's evidence; its private Withdrawn fact leaks from the exact two-relay test instead of preserving only the outward wire close.
  Scenario: A finite semantic operation closes after every owned source and destination is terminal
    Given relay 1 and relay 2 each own one exact finite source request
    And relay 1 has settled while relay 2 remains unfinished
    When a qualified successor arrives through relay 2's active owned request
    And NMP restarts before relay 2 settles
    Then only relay 2's unfinished source request is reopened
    And stale or unrelated request evidence cannot settle or resurrect the source round
    And after relay 2 and every current destination become terminal the operation cohort settles atomically

  # nmp:id=WRITES-REPLACEABLE-EDIT-023
  # nmp:status=built
  # nmp:evidence=rust:nmp::finite_source_policy_reuses_advanced_round_and_refuses_every_policy_change
  # nmp:falsifier=Rebuild a fresh finite round when the second operation declares the same relay/access set; the already-open request becomes pending again and can be reopened or settled twice.
  Scenario: Later operations reuse the exact finite source round
    Given a semantic resource has a finite source round with one request already open
    When another operation declares the same relay and access set over the current pending generation
    Then acceptance keeps the original round identity and request evidence
    And changing the source lifetime, relay set, or access is refused before custody

  # nmp:id=WRITES-REPLACEABLE-EDIT-020
  # nmp:status=built
  # nmp:evidence=rust:nmp::route_only_addition_preserves_signed_e2_and_sends_only_the_new_destination
  # nmp:falsifier=Re-enqueue every destination when routing knowledge grows instead of only the new exact event-relay lane; relay A or relay B receives E2 more than once.
  @acceptance
  Scenario: A later destination receives the same signed generation without resending completed destinations
    Given semantic generation E2 is signed and relay A has accepted it
    And E2 is still waiting for one recipient's relay list
    When that relay list adds relay B as a destination
    Then relay B receives the exact same E2 event id and signature once
    And relay A does not receive E2 again

  # nmp:id=WRITES-REPLACEABLE-EDIT-022
  # nmp:status=built
  # nmp:evidence=rust:nmp::source_session_replacement_wakes_every_signed_successor_destination
  # nmp:evidence=rust:nmp::shared_second_generation_is_once_per_relay_and_replays_without_settling
  # nmp:evidence=rust:nmp-store::qualified_source_and_complete_successor_survive_redb_reopen
  # nmp:falsifier=Re-bootstrap current E2 lanes against retained terminal E1 attempt history; recovery refuses the valid successor lanes and both destinations remain Waiting(NotConnected).
  @acceptance
  Scenario: A signed successor survives predecessor session replacement
    Given relay A and relay B accepted terminal generation E1
    And relay B supplies a newer source that creates signed generation E2
    When predecessor write sessions are replaced by the current E2 generation
    Then relay A and relay B each receive E2 exactly once

  # ---- the precondition --------------------------------------------------

  # nmp:id=WRITES-REPLACEABLE-EDIT-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::replaceable_base_precondition_accepts_the_exact_winner_and_none_means_none
  # nmp:falsifier=Refuse an edit whose expected base is the exact stored winner, or retain that winner as current after acceptance; the store transaction proof fails.
  Scenario: An edit naming the stored version replaces it
    Given my device clock reads "2026-07-29T12:00:10Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is accepted
    And the replacement is the stored winner
    And "wss://hub.example" received the replacement

  # nmp:id=WRITES-REPLACEABLE-EDIT-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::replaceable_base_precondition_rejects_a_concurrent_winner_atomically
  # nmp:evidence=rust:nmp-store::a_refused_write_is_taken_into_custody_as_one_permanently_failed_receipt
  # nmp:evidence=rust:nmp::stale_replaceable_edit_is_refused_into_custody_keeping_both_event_ids
  # nmp:falsifier=Overwrite the changed winner, drop receipt-only custody, lose either competing event id, or create signing/routing/delivery work; the atomic store, durable receipt, core, or BDD proof fails.
  Scenario: A concurrent edit that moved the winner is refused, not overwritten
    # The headline. Two devices editing the same list is the ordinary case,
    # not the exotic one, and the wrong outcome here is not an error -- it is
    # the other device's change vanishing without anybody being told.
    Given my device clock reads "2026-07-29T12:00:10Z"
    And another device replaced it with "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" created at "2026-07-29T12:00:30Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is refused with a replaceable conflict
    And the conflict names "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as expected and "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" as actual
    And the stored winner is still "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990"
    And the refused write remains one terminal receipt with no signing, routing, delivery, or retry work
    And "wss://hub.example" received nothing

  # nmp:id=WRITES-REPLACEABLE-EDIT-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::replaceable_base_precondition_rejects_a_concurrent_winner_atomically
  # nmp:falsifier=Validate the base before the acceptance transaction instead of against its current row; a concurrent replacement is overwritten rather than refused.
  Scenario: The check is against the row at acceptance, not the row the app read
    # What "atomically at acceptance" buys. The app's read was correct when
    # it happened; the winner moved afterwards, while the write was in
    # flight. A precondition evaluated at compose time would have passed and
    # then clobbered.
    Given my device clock reads "2026-07-29T12:00:10Z"
    When I read the stored winner and compose a replacement naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    And another device replaces it with "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" before my write is accepted
    And I publish that replacement
    Then the write is refused with a replaceable conflict
    And the stored winner is still "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990"

  # ---- the stamp ---------------------------------------------------------

  # nmp:id=WRITES-REPLACEABLE-EDIT-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_restamped_replaceable_edit_reports_its_post_restamp_id
  # nmp:falsifier=Derive the replacement timestamp from the app's stale copy rather than the accepted stored winner; the returned post-restamp id no longer reflects the monotonic store stamp.
  Scenario: A replacement is stamped against the stored version, not the stale copy the app holds
    # The case the whole design turns on. The app was holding the 12:00:00
    # version, the store holds a 12:00:30 one, and the app's own clock reads
    # 12:00:10 -- so every number the app could have stamped with is behind
    # the version being replaced, and any of them would have produced an edit
    # that loses. The refusal tells the app to re-read; the stamp is then
    # computed against the row the precondition is holding, and the
    # replacement lands correctly ordered.
    Given my device clock reads "2026-07-29T12:00:10Z"
    And another device replaced it with "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" created at "2026-07-29T12:00:30Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is refused with a replaceable conflict
    When I re-read the stored winner and publish a replacement naming "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" as the version it replaces
    Then the write is accepted
    And the replacement's created_at is "2026-07-29T12:00:31Z"
    And the replacement's created_at is greater than "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990"'s
    And the replacement is the stored winner

  # nmp:id=WRITES-REPLACEABLE-EDIT-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_restamped_replaceable_edit_reports_its_post_restamp_id
  # nmp:falsifier=Use a behind wall clock without advancing past the stored winner; the replacement id is not derived from winner timestamp plus one.
  Scenario: A clock behind the stored version cannot produce a losing replacement
    # The same rule with no conflict in it, so the stamp is the only thing
    # under test. A device whose clock is wrong still edits its own contact
    # list successfully, because the stamp is max(clock, winner + 1) and the
    # winner is read from inside the transaction.
    Given my device clock reads "2026-07-29T11:59:50Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is accepted
    And the replacement's created_at is "2026-07-29T12:00:01Z"
    And the replacement is the stored winner

  # nmp:id=WRITES-REPLACEABLE-EDIT-006
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::replaceable_base_precondition_accepts_the_exact_winner_and_none_means_none
  # nmp:falsifier=Restamp an already-newer replacement merely because it is replaceable; the accepted event no longer preserves the caller's winning timestamp.
  Scenario: A clock ahead of the stored version is used as it stands
    # The other branch of the same max. NMP is not rewriting time, it is
    # refusing to go backwards; when the clock is already ahead there is
    # nothing to correct.
    Given my device clock reads "2026-07-29T12:05:00Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is accepted
    And the replacement's created_at is "2026-07-29T12:05:00Z"

  # nmp:id=WRITES-REPLACEABLE-EDIT-007
  # nmp:status=built
  # nmp:evidence=rust:nmp-grammar::a_kind_alone_is_a_complete_builder
  # nmp:falsifier=Restamp an explicitly stated created_at; the write grammar no longer preserves every caller-owned builder field.
  Scenario: An app that states its own created_at keeps it, even when that loses
    # A foot-gun deliberately left loaded. A builder can provide anything and
    # that does not stop being true here, so a caller-stated timestamp is
    # honoured verbatim -- including one that regresses below the winner and
    # loses the replacement race. The failure stays observable rather than
    # forbidden; what NMP must never do is quietly "fix" it, because
    # present-then-changed is the one thing a stated field may never be.
    Given my device clock reads "2026-07-29T12:00:10Z"
    When I publish a replacement contact list created at "2026-07-29T11:00:00Z" naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is accepted
    And the replacement's created_at is "2026-07-29T11:00:00Z"
    And nothing restamped it to "2026-07-29T12:00:01Z"

  # ---- somebody else's version -------------------------------------------

  # nmp:id=WRITES-REPLACEABLE-EDIT-008
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::replaceable_base_precondition_rejects_a_concurrent_winner_atomically
  # nmp:falsifier=Compare the expected base outside the resolved write-author coordinate; an event from another author can authorize mutation of the current account's row.
  Scenario: Editing a replaceable event somebody else authored fails the precondition
    # No dedicated wrong-author error, and none is wanted. The precondition
    # is checked at MY coordinate, where Carol's contact list is not and
    # never will be the winner, so the base is unsatisfiable and says so
    # through the conflict door every other stale base uses.
    Given "4c26d9074c27d89ede59270c0ac14b71e071b15239519f75474b2f3ba63481f5"'s contact list "3671101a76907dac61faee04464f38138e411c385ebb62cb34e756cd8239d7b8" is stored locally
    And my device clock reads "2026-07-29T12:00:10Z"
    When I publish a replacement contact list naming "3671101a76907dac61faee04464f38138e411c385ebb62cb34e756cd8239d7b8" as the version it replaces
    Then the write is refused with a replaceable conflict
    And the conflict names "3671101a76907dac61faee04464f38138e411c385ebb62cb34e756cd8239d7b8" as expected and "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as actual
    And "4c26d9074c27d89ede59270c0ac14b71e071b15239519f75474b2f3ba63481f5"'s contact list is unchanged
    And my own contact list is still "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe"
    And "wss://hub.example" received nothing

  # nmp:id=WRITES-REPLACEABLE-EDIT-009
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_explicit_identity_publishes_as_a_secondary_without_moving_the_current_account
  # nmp:evidence=rust:nmp-store::replaceable_base_precondition_accepts_the_exact_winner_and_none_means_none
  # nmp:falsifier=Resolve the replaceable coordinate from session selection instead of the write's frozen author; an explicit secondary identity mutates the wrong account's winner.
  Scenario: The coordinate follows the identity the write publishes as
    # Which coordinate gets checked is decided by the same identity
    # resolution that decides the author -- so a write naming the podcast
    # identity is checked against the PODCAST identity's contact list, not
    # against the current account's. If the coordinate came from anywhere
    # else, publishing as one identity could CAS against another's row.
    Given my podcast identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" has an available signing provider
    And that identity's contact list "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" created at "2026-07-29T12:00:30Z" is its stored winner
    And my device clock reads "2026-07-29T12:00:40Z"
    When I publish a replacement contact list naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" and "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" as the version it replaces
    Then the write is accepted
    And the replacement is the stored winner for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And my own contact list is still "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe"
