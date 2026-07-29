Feature: A named identity with no signer waits; it does not fail
  Naming a key NMP cannot currently sign for is not an error. It is a
  complete, self-sufficient statement of intent with exactly one piece
  missing, and the missing piece is the one piece that can arrive later. The
  author is known, so the body can be frozen and written down. Only the
  capability is absent.

  Two workflows exist entirely inside that gap. An app can queue writes as an
  identity before that identity's signer is connected -- a remote signer that
  finishes pairing a minute after the user hit send, a hardware signer that
  is plugged in when the user gets to it. And an app can publish as a key it
  holds while logged out of everything, which is only useful if "logged out"
  does not also mean "no signer attached yet". Failing the write would delete
  both, and would do it in the least recoverable way: the app finds out at
  the moment it can do nothing about it.

  Waiting is not limbo. A parked write is reported as parked, on the same
  receipt stream as everything else, naming the exact key it is waiting for.
  It survives restart, because a decision the app made and NMP acknowledged
  cannot quietly not survive a restart. And it can be cancelled, which is
  what makes it a decision the app owns rather than a resource NMP is holding
  on its behalf.

  The contrast that makes this coherent is in write-identity.feature: a write
  with no active account and no named identity FAILS, and fails before
  acceptance. The difference is not severity, it is whether there is anything
  to wait for. A named key is something concrete; "whoever is active" with
  nobody active is nothing at all, so there is nothing to park and nothing to
  attach later.

  Background:
    Given the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is registered with a working signer
    And "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the active account
    And no signer is registered for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And my relay list names "wss://hub.example" as my write relay

  # ---- parking -----------------------------------------------------------

  Scenario: Naming an identity with no registered signer parks the write
    # The headline. The write is accepted -- a real, durable acceptance with
    # a frozen body -- and then waits. It is not refused, and it is not
    # quietly rerouted to the account that does have a signer.
    When I compose an event of kind 1 saying "episode 17 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the write reports accepted
    And the receipt reports it awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write is never refused
    And "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is never asked to sign it
    And "wss://hub.example" received nothing yet

  Scenario: The park is visible, not inferred from silence
    # The failure this rules out is a write that looks live and is actually
    # stuck. The app must be able to render "waiting for your podcast signer"
    # without guessing from the absence of a receipt, so the key being waited
    # on is on the receipt itself.
    When I compose an event of kind 1 saying "episode 18 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the receipt names "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" as the key it is waiting for
    And the receipt can be reattached by its stable id

  # ---- durability --------------------------------------------------------

  Scenario: A parked write survives restart still waiting for the same key
    When I compose an event of kind 1 saying "episode 19 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write reports accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    Then the write is still awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And its frozen body is byte-for-byte what it was before the restart

  # ---- completion --------------------------------------------------------

  Scenario: The write completes when that key's signer attaches later
    # The workflow the park exists for. A remote signer finishes pairing well
    # after the app queued the write, and the write picks up exactly where it
    # was -- same frozen body, same author, no recompose.
    When I compose an event of kind 1 saying "episode 20 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the receipt reports it awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And a NIP-46 signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" attaches 30 seconds later
    Then the write is signed by that signer
    And the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received it

  Scenario: A parked write survives restart and completes on the far side of it
    # Both halves in one run, because the two properties are only worth
    # having together: surviving a restart is pointless if the reattach no
    # longer re-arms the write, and re-arming is pointless if the write did
    # not survive.
    When I compose an event of kind 1 saying "episode 21 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write reports accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    And a NIP-46 signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" attaches
    Then the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received it

  Scenario: A signer for a different key does not unpark it
    # The park names one key. Any other signer arriving is not the one it is
    # waiting for, however convenient it would be, and the frozen author is
    # not up for renegotiation because something showed up.
    When I compose an event of kind 1 saying "episode 22 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And a NIP-46 signer for "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" attaches
    Then the write is still awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received nothing

  Scenario: Logging out and back in as somebody else leaves the park alone
    # A parked write is not a property of the session. The account switch is
    # exactly the event that would retarget it if the identity were not
    # already pinned.
    When I compose an event of kind 1 saying "episode 23 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And I switch the active account to "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9"
    Then the write is still awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"

  # ---- revoking it -------------------------------------------------------

  Scenario: A parked write can be cancelled
    # What makes waiting indefinitely acceptable: the app is never stuck with
    # it. Cancelling is the app's own decision arriving later, the same way
    # the signer would have.
    When I compose an event of kind 1 saying "episode 24 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the receipt reports it awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And I cancel that write
    Then the write is reported cancelled
    When a NIP-46 signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" attaches
    Then nothing is signed
    And "wss://hub.example" received nothing
