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
  # nmp:issue=#1432
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
  # nmp:falsifier=Accept an operation from a replaced registration or retain anything after synchronous materializer refusal; the queue is no longer empty and the signed source is no longer the sole canonical row.
  # nmp:issue=#1432
  Scenario: An unavailable capability refuses the operation before custody
    Given the capability required by the operation is not configured
    When I try to add Alice to my contact list through that capability
    Then publishing is refused with a typed configuration error
    And NMP retains no receipt, write intent, optimistic row, signing work, route, delivery work, or correlation

  # The encrypted content is opaque to an operation that owns only a public
  # tag. Its presence does not turn that operation into a crypto operation.

  # nmp:id=WRITES-REPLACEABLE-EDIT-012
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip02::alice_then_bob_keep_two_receipts_and_one_complete_pending_event
  # nmp:evidence=rust:nmp-nip02::semantic_operations_compose_in_order_and_preserve_unowned_fields
  # nmp:falsifier=Rewrite or decrypt content while applying the public follow operation; the exact opaque source content no longer survives both materialization and durable restart.
  # nmp:issue=#1432
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
  # nmp:issue=#1432
  Scenario: Several offline operations keep their receipts while sharing one complete current event
    Given I am disconnected from every relay
    When a configured capability adds Alice to my contact list
    And the configured capability then adds Bob to my contact list
    Then both operations have distinct ordinary receipts
    And one complete current signature-pending event contains Alice and Bob
    And both receipts name that current event without creating another receipt lifecycle

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
  # nmp:evidence=rust:nmp::stale_replaceable_edit_is_refused_into_custody_keeping_both_event_ids
  # nmp:falsifier=Overwrite a winner that changed after the app read it or lose either competing event id; the atomic store or receipt proof fails.
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
    And nothing was journaled and no event id was allocated
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
