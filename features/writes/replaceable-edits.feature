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

  # ---- the precondition --------------------------------------------------

  @designed
  Scenario: An edit naming the stored version replaces it
    Given my device clock reads "2026-07-29T12:00:10Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is accepted
    And the replacement is the stored winner
    And "wss://hub.example" received the replacement

  @designed
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

  @designed
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

  @designed
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

  @designed
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

  @designed
  Scenario: A clock ahead of the stored version is used as it stands
    # The other branch of the same max. NMP is not rewriting time, it is
    # refusing to go backwards; when the clock is already ahead there is
    # nothing to correct.
    Given my device clock reads "2026-07-29T12:05:00Z"
    When I publish a replacement contact list naming "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" as the version it replaces
    Then the write is accepted
    And the replacement's created_at is "2026-07-29T12:05:00Z"

  @designed
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

  @designed
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

  @designed
  Scenario: The coordinate follows the identity the write publishes as
    # Which coordinate gets checked is decided by the same identity
    # resolution that decides the author -- so a write naming the podcast
    # identity is checked against the PODCAST identity's contact list, not
    # against the active account's. If the coordinate came from anywhere
    # else, publishing as one identity could CAS against another's row.
    Given my podcast identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" is registered with a working signer
    And that identity's contact list "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" created at "2026-07-29T12:00:30Z" is its stored winner
    And my device clock reads "2026-07-29T12:00:40Z"
    When I publish a replacement contact list naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" and "fb04dcb6970e4c3d1873de51fd5a50d7bb46b3383113602665c350ec40b5f990" as the version it replaces
    Then the write is accepted
    And the replacement is the stored winner for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And my own contact list is still "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe"
