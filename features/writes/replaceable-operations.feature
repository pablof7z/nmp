Feature: A registered replaceable operation is derived once, at acceptance, from whatever source NMP currently holds
  A capability-owned operation names a change, not a byte-for-byte event: "follow
  Alice", not "publish this exact kind:3". The configured capability materializes
  the complete replacement synchronously, against the best source NMP currently
  has -- offline, that is the capability's own first-value policy; once a newer
  relay source arrives, NMP re-runs the same materializer over it and installs a
  successor generation, preserving every still-open operation's receipt identity
  across the replacement.

  This removes the seam a caller-composed replacement cannot close on its own:
  no read/compose/publish loop can pick a correct timestamp or base when a newer
  source can arrive between any two of its own steps, because the timestamp and
  the source it is measured against are decided together, inside the one
  transaction that installs the generation.

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
  # nmp:evidence=rust:nmp::shared_second_generation_is_once_per_relay_and_replays_while_a_destination_is_down
  # nmp:falsifier=Suppress the physical owner's E2 Signing(Signed) receipt fact while leaving E2 delivery intact; the original contributing receipt no longer observes the shared generation signature.
  Scenario: Shared operation receipts observe one physical generation delivery
    Given Alice and Bob have distinct operation receipts sharing current generation E2
    And their destination plans overlap
    When E2 is signed and delivered
    Then exactly one signer request and one physical publication per relay occur for E2
    And both receipts expose signing and relay evidence naming E2

  # nmp:id=WRITES-REPLACEABLE-EDIT-019
  # nmp:status=built
  # nmp:evidence=rust:nmp::relay_source_successors_resume_current_delivery_and_stay_open_after_restart
  # nmp:falsifier=Treat a relay replay of terminal E2 as a new semantic source after restart; it supersedes signed E3 before E3 reaches every existing destination.
  Scenario: An unreachable destination keeps a semantic operation open
    Given one routed destination for the current semantic generation is unreachable
    When every other destination becomes terminal
    Then each operation receipt remains open with event-qualified terminal relay evidence
    And a later qualified source may still create one successor generation
    And no terminal receipt is resurrected

  # nmp:id=WRITES-REPLACEABLE-EDIT-021
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_delivered_semantic_write_settles_its_receipt
  # nmp:falsifier=Refuse the cohort close while every routed lane is terminal; the delivered follow's receipt never reports Settled and its durable semantic state survives.
  @acceptance
  Scenario: A semantic operation settles once routing is closed and every lane is terminal
    Given a follow is routed to its destinations
    When every lane of the current generation becomes terminal
    Then every contributing operation receipt settles atomically
    And the durable semantic resource and its replay program are removed
    And a later unrelated list does not recreate the action

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
