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

    Scenario: One session restores signer-backed and public-key-only accounts
      Given a session contains a private-key account and a public-key-only account
      And one account is current for reactive queries
      When the opaque session is encoded and restored by a compatible build
      Then both accounts are restored
      And the same current public key is restored
      And the app never interprets sensitive restoration material

    Scenario: Restore is atomic when one provider description is invalid
      Given a session contains an account with a stored provider configuration and another account
      And the current build rejects the provider version
      When the session is restored
      Then restoration is refused with the exact provider and version reason
      And no account, provider, or current public key becomes active

  Rule: Removing session state never rewrites accepted history

    Scenario: Removing an account has one complete meaning
      Given the session contains a current signer-backed account
      And an accepted write is waiting for its public key
      When that exact account is removed
      Then the account and signer provider are absent
      And the current public key is cleared
      But cached events, receipts, and the accepted write remain
      And the write reports that it is awaiting its exact frozen public key

    Scenario: Clearing the session does not clear NMP data or accepted obligations
      Given a session has several accounts and a current public key
      And an accepted write is waiting for one of them
      When the session is cleared
      Then no account, signer provider, or current public key remains in the session
      But cached events, receipts, and the accepted write remain
      And the write reports that it is awaiting its exact frozen public key
