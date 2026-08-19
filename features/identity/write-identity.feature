Feature: A write publishes as the current account unless it names someone else
  Every write has exactly one identity, and there are exactly two ways to
  arrive at it: the current account, or a key the app named. Naming nobody is
  not the absence of a choice -- it is the choice "whoever is current when
  this is accepted", which is a positive instruction that can succeed, fail,
  or be pinned, and which shows up in receipts and diagnostics where a blank
  would say nothing.

  The overwhelming majority of writes are the first kind, so the first kind
  costs nothing to say. An app that is logged in and wants to post as itself
  writes down what it wants to post and stops.

  The second kind exists because an app may hold several identities at once,
  and because publishing as one of them must not require making it the current
  one. A podcast identity posts an episode without the user's main account
  ever leaving the screen. This works while logged out entirely: naming a key
  is a complete statement of intent on its own, and does not borrow anything
  from a session.

  Whichever way the identity was arrived at, it is fixed the moment the write
  is accepted -- resolved to one concrete key, frozen into the body, and
  never revisited. This is the load-bearing rule of the whole surface. A
  queued write whose author floated with the current session would retarget
  itself on the next account switch, and the app that queued it would have no
  way to know. Under a current-account default the pin matters MORE than it
  used to, not less: the default is precisely the case where the app never
  named a key, so acceptance is the only place "whoever is current" becomes
  somebody.

  An identity is always a public key. It is never an npub, and never any
  other bech32 form. Bech32 is how something is shown to a person or received
  from one; an app that took an npub from a paste box decodes it at that
  boundary, where the display form actually arrived, and hands NMP a key.
  Accepting both encodings here would be a convenience that costs one
  answer to "what does NMP take", and doubles the error surface of every
  pubkey-shaped input after it.

  Background:
    Given the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" has an available signing provider
    And the account with pubkey "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" has an available signing provider
    And my podcast identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" has an available signing provider
    And my relay list names "wss://hub.example" as my write relay

  # ---- the default -------------------------------------------------------

  Scenario: A write that names no identity publishes as the current account
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    When I compose an event of kind 1 saying "hello" and publish it naming no identity
    Then the published event is authored by "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And it was signed by that account's signer

  Scenario: The default follows the current account, it does not remember the first one
    # "Whoever is current at acceptance" resolved twice, against two different
    # answers. This is what distinguishes a resolution instruction from a
    # value captured once at startup.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    When I compose an event of kind 1 saying "first" and publish it naming no identity
    And I make "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" the current account
    And I compose an event of kind 1 saying "second" and publish it naming no identity
    Then "first" is authored by "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And "second" is authored by "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9"

  # ---- naming an identity ------------------------------------------------

  Scenario: A write can name an identity that is not the current account
    # The podcast case. Publishing as one identity must not require making it
    # the current one, because changing the current account affects everything else
    # on screen.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    When I compose an event of kind 1 saying "episode 12 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And it was signed by the podcast identity's signer
    And "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is still the current account

  Scenario: A named identity publishes while no account is current
    # Naming a key is self-sufficient: the author is known, the signer is
    # registered, and nothing about the write needs a session. An app that
    # requires login before it will publish as a key it holds is adding a
    # requirement that does not come from anywhere.
    Given no account is current
    When I compose an event of kind 1 saying "episode 13 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received it

  # ---- the pin -----------------------------------------------------------

  Scenario: Switching accounts never retargets a write that was already accepted
    # The named-identity half. The switch happens while the write is still in
    # flight, and it changes nothing about it -- not the author, not which
    # signer is asked.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    And the podcast identity's signing provider is slow to answer
    When I compose an event of kind 1 saying "episode 14 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write reports accepted
    And I make "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" the current account
    Then the pending write still awaits "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And neither "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" nor "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" is asked to sign it
    When the podcast identity's signing provider answers
    Then the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"

  Scenario: A current-account write is pinned to whoever was current when it was accepted
    # The half that only exists because of this design, and the one most
    # likely to regress. The app never named a key, so nothing in the
    # write's own text says who it belongs to -- acceptance is where that
    # gets decided, and after acceptance it is decided.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    And that account's signing provider is slow to answer
    When I compose an event of kind 1 saying "hello" and publish it naming no identity
    And the write reports accepted
    And I make "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" the current account
    Then the pending write still awaits "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" is never asked to sign it
    When the first account's signing provider answers
    Then the published event is authored by "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"

  Scenario: A restart does not re-resolve an accepted write against the new session
    # The pin has to survive the process, not just the switch. Replay from
    # the journal must reload a decided identity, never re-run the
    # resolution against whichever account happens to be current on the way
    # back up.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    And that account's signing provider is unavailable
    When I compose an event of kind 1 saying "hello" and publish it naming no identity
    And the write reports accepted and the process stops immediately
    And I reconstruct the engine from the same durable store with "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" current
    Then the pending write still awaits "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"

  # ---- failing closed ----------------------------------------------------

  Scenario: No current account and no named identity fails before acceptance
    # Nothing is pinned, so nothing may park. "Whoever is current" with nobody
    # current names no one, and there is no key to wait for -- so this is a
    # refusal, not a parked hope. Contrast awaiting-signer.feature, where a
    # named key is something concrete to wait for and the write parks
    # instead.
    Given no account is current
    When I compose an event of kind 1 saying "hello" and publish it naming no identity
    Then the write is refused for having no identity to publish as
    And it never reports accepted
    And no journal row was written and no write id was allocated
    And "wss://hub.example" received nothing

  # ---- bech32 stops at the app's boundary --------------------------------

  Scenario: An app that holds an npub decodes it before it reaches the write plane
    # The intended path, and the reason the refusal below costs apps nothing.
    # The decode happens where the display form actually arrived -- the paste
    # box -- using the same bech32 door every other entity goes through.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    And the user pasted the npub form of "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" into the identity picker
    When the app decodes it to a public key
    And I compose an event of kind 1 saying "episode 15 is up" and publish it naming that identity
    Then the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"

  Scenario: An identity given in bech32 is refused, however well-formed it is
    # Not a parsing failure -- a boundary rule. The string is a perfectly
    # valid npub for an identity that really is registered here, and it is
    # still refused, because "which encodings does this field take" must have
    # one answer forever rather than one answer per field.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    When I compose an event of kind 1 saying "episode 16 is up" and publish it naming as identity the npub form of "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    Then the write is refused for not being given a public key
    And "wss://hub.example" received nothing
