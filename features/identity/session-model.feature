Feature: One restored session contains every known account
  An engine session is the complete set of accounts the app has added to that
  engine, plus the one account currently selected for reactive queries. Every
  account has a public key. It may also have one signer provider, but a public
  key without a signer is still an ordinary account and must survive restart.

  A provider's configuration belongs to the session; its momentary ability to
  sign does not. Losing a relay connection, remote service, or device connection
  makes the provider unavailable. It does not remove the account, invalidate
  another account, or turn availability into a second kind of account.

  Removing an account has one complete meaning: the account and its provider
  leave together. There is no separate remove-signer operation. Writes already
  accepted for that public key are separate durable obligations, so removal or
  clearing the session cannot retarget or delete them.

  Rule: Restoration reconstructs the whole structurally valid session

    # nmp:id=IDENTITY-SESSION-MODEL-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::whole_session_round_trip_is_canonical_and_restores_public_only_accounts
    # nmp:evidence=swift:NMP::testWholeSessionRoundTripsSignerBackedAndPublicKeyOnlyAccounts
    # nmp:evidence=kotlin:NMPKotlin::wholeSessionRestoresSignerBackedAndPublicKeyOnlyAccounts
    # nmp:falsifier=Drop providerless accounts or the current public key from the canonical payload, or make the app decode that payload; the native round-trip or Kotlin restore projection no longer reconstructs the same complete session.
    Scenario: One session restores signer-backed and public-key-only accounts
      Given a session contains a private-key account and a public-key-only account
      And one account is current for reactive queries
      When the opaque session is encoded and restored by a compatible build
      Then both accounts are restored
      And the same current public key is restored
      And the app never interprets sensitive restoration material

    # nmp:id=IDENTITY-SESSION-MODEL-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::malformed_restore_creates_no_partially_visible_engine
    # nmp:evidence=rust:nmp::every_restore_refusal_is_reachable_and_typed
    # nmp:falsifier=Install any account before every provider description validates, or collapse a provider-version refusal into malformed bytes; one of the restore proofs exposes partial state or loses the exact refusal axis.
    Scenario: Restore is atomic when one provider description is invalid
      Given a session contains an account with a stored provider configuration and another account
      And the current build rejects the provider version
      When the session is restored
      Then restoration is refused with the exact provider and version reason
      And no account, provider, or current public key becomes active

  Rule: Removing session state never rewrites accepted history

    # nmp:id=IDENTITY-SESSION-MODEL-004
    # nmp:status=built
    # nmp:evidence=rust:nmp::remove_current_account_clears_current_in_same_runtime_turn
    # nmp:evidence=rust:nmp::removing_or_clearing_session_never_retargets_or_discards_accepted_writes
    # nmp:evidence=kotlin:NMPKotlin::removingCurrentAccountClearsSelectionAndRemovalHasOneMeaning
    # nmp:falsifier=Leave the removed account current, remove only its provider, or mutate its accepted write identity; the runtime or Kotlin proof observes split account ownership or a retargeted obligation.
    Scenario: Removing an account has one complete meaning
      Given the session contains a current signer-backed account
      And an accepted write is waiting for its public key
      When that exact account is removed
      Then the account and signer provider are absent
      And the current public key is cleared
      But cached events, receipts, and the accepted write remain
      And the write reports that it is awaiting its exact frozen public key

    # nmp:id=IDENTITY-SESSION-MODEL-005
    # nmp:status=built
    # nmp:evidence=rust:nmp::session_mutations_update_one_account_and_clear_the_whole_value
    # nmp:evidence=rust:nmp::removing_or_clearing_session_never_retargets_or_discards_accepted_writes
    # nmp:evidence=kotlin:NMPKotlin::makeCurrentAndClearOperateOnTheWholeSession
    # nmp:falsifier=Let clear retain an account/current key or delete cached and accepted-write state with the session; the native preservation proof or Kotlin whole-session projection fails.
    Scenario: Clearing the session does not clear NMP data or accepted obligations
      Given a session has several accounts and a current public key
      And an accepted write is waiting for one of them
      When the session is cleared
      Then no account, signer provider, or current public key remains in the session
      But cached events, receipts, and the accepted write remain
      And the write reports that it is awaiting its exact frozen public key
