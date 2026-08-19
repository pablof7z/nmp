Feature: A named identity with no available signing provider waits; it does not fail
  Naming a key NMP cannot currently sign for is not an error. It is a
  complete, self-sufficient statement of intent with exactly one piece
  missing, and the missing piece is the one piece that can arrive later. The
  author is known, so the body can be frozen and written down. Only the
  capability is absent.

  Two workflows exist entirely inside that gap. An app can queue writes while
  that identity's provider is unavailable -- a remote provider whose relay is
  unreachable, or a hardware provider whose device is disconnected. And an app
  can publish as a key it holds while no account is current, which is only useful
  if "no current account" does not also mean "no provider may become available". Failing the write would delete
  both, and would do it in the least recoverable way: the app finds out at
  the moment it can do nothing about it.

  Waiting is not limbo. A parked write is reported as parked, on the same
  receipt stream as everything else, naming the exact key it is waiting for.
  It survives restart, because a decision the app made and NMP acknowledged
  cannot quietly not survive a restart. And it can be cancelled, which is
  what makes it a decision the app owns rather than a resource NMP is holding
  on its behalf.

  The contrast that makes this coherent is in write-identity.feature: a write
  with no current account and no named identity FAILS, and fails before
  acceptance. The difference is not severity, it is whether there is anything
  to wait for. A named key is something concrete; "whoever is current" with
  nobody current is nothing at all, so there is nothing to park.

  Background:
    Given the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" has an available signing provider
    And "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    And the account with pubkey "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" has no signing provider
    And my relay list names "wss://hub.example" as my write relay

  # ---- parking -----------------------------------------------------------

  # nmp:id=IDENTITY-AWAITING-PROVIDER-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-ffi::ffi_explicit_identity_for_unregistered_pubkey_parks_awaiting_capability
  # nmp:evidence=rust:nmp::parked_awaiting_capability_reattach_cancel_does_not_retain_deliveries
  # nmp:falsifier=Refuse an explicit public key merely because it has no available provider, or ask the current account to sign it; the facade or runtime proof no longer observes an accepted write pinned to the named key.
  Scenario: Naming an identity with no signing provider parks the write
    # The headline. The write is accepted -- a real, durable acceptance with
    # a frozen body -- and then waits. It is not refused, and it is not
    # quietly rerouted to the account that does have a signer.
    When I compose an event of kind 1 saying "episode 17 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the write reports accepted
    And the receipt reports it awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write is never refused
    And "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is never asked to sign it
    And "wss://hub.example" received nothing yet

  # nmp:id=IDENTITY-AWAITING-PROVIDER-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-ffi::ffi_explicit_identity_for_unregistered_pubkey_parks_awaiting_capability
  # nmp:evidence=rust:nmp::parked_awaiting_capability_reattach_cancel_does_not_retain_deliveries
  # nmp:falsifier=Omit the frozen public key from the awaiting-signature fact or make its receipt unreattachable; the facade projection or runtime replay proof loses observable custody.
  Scenario: The park is visible, not inferred from silence
    # The failure this rules out is a write that looks live and is actually
    # stuck. The app must be able to render "waiting for your podcast signer"
    # without guessing from the absence of a receipt, so the key being waited
    # on is on the receipt itself.
    When I compose an event of kind 1 saying "episode 18 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the receipt names "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" as the key it is waiting for
    And the receipt can be reattached by its stable id

  # ---- durability --------------------------------------------------------

  # nmp:id=IDENTITY-AWAITING-PROVIDER-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::removing_or_clearing_session_never_retargets_or_discards_accepted_writes
  # nmp:falsifier=Re-resolve a recovered accepted write against the restored current account or discard its frozen body; recovery no longer preserves the one accepted obligation and exact public key.
  Scenario: A parked write survives restart still waiting for the same key
    When I compose an event of kind 1 saying "episode 19 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write reports accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    Then the write is still awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And its frozen body is byte-for-byte what it was before the restart

  # nmp:id=IDENTITY-AWAITING-PROVIDER-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::removing_or_clearing_session_never_retargets_or_discards_accepted_writes
  # nmp:evidence=rust:nmp::an_explicit_identity_publishes_as_a_secondary_without_moving_the_current_account
  # nmp:falsifier=Let session selection rewrite the public key frozen into an accepted write; removing, clearing, or selecting another account makes either the preserved obligation or secondary-identity proof fail.
  Scenario: Logging out and back in as somebody else leaves the park alone
    # A parked write is not a property of the session. The account switch is
    # exactly the event that would retarget it if the identity were not
    # already pinned.
    When I compose an event of kind 1 saying "episode 23 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And I make "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" the current account
    Then the write is still awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"

  # ---- revoking it -------------------------------------------------------

  # nmp:id=IDENTITY-AWAITING-PROVIDER-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::parked_awaiting_capability_reattach_cancel_does_not_retain_deliveries
  # nmp:evidence=rust:nmp::facade_cancellation_is_typed_idempotent_and_reattachable
  # nmp:falsifier=Let cancellation leave the signing request live or erase the terminal receipt; a later provider can sign, or the cancelled outcome cannot be reattached.
  Scenario: A parked write can be cancelled
    # What makes waiting indefinitely acceptable: the app is never stuck with
    # it. Cancelling is the app's own decision arriving later, the same way
    # the provider recovery would have.
    When I compose an event of kind 1 saying "episode 24 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the receipt reports it awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And I cancel that write
    Then the write is reported cancelled
    When the NIP-46 signing provider for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" becomes available
    Then nothing is signed
    And "wss://hub.example" received nothing
