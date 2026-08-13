Feature: An unavailable NIP-46 provider can resume its exact writes
  NIP-46 support will restore an account and its provider configuration even
  while the provider's relay dependency is unreachable. The account remains
  part of the session, reports its signing capability as unavailable, and does
  not block other accounts. A write already accepted for its public key stays
  pinned to that key until the configured provider becomes available.

  Background:
    Given the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" has an available signing provider
    And "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the current account
    And the account with pubkey "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" has an unavailable NIP-46 signing provider
    And my relay list names "wss://hub.example" as my write relay

  # nmp:id=IDENTITY-NIP46-RECOVERY-004
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1169
  Scenario: A restored unavailable provider remains part of the session
    Given a session contains an account whose NIP-46 provider requires a reachable relay or remote service
    When the engine restores while that dependency is unavailable
    Then the account and provider configuration remain restored
    And its signing capability reports unavailable
    And other restored accounts remain usable
    And any accepted write for its public key remains awaiting that exact provider

  # nmp:id=IDENTITY-NIP46-RECOVERY-001
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1169
  Scenario: The write completes when that key's provider becomes available later
    When I compose an event of kind 1 saying "episode 20 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the receipt reports it awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the NIP-46 signing provider for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" becomes available 30 seconds later
    Then the write is signed by that provider
    And the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received it

  # nmp:id=IDENTITY-NIP46-RECOVERY-002
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1169
  Scenario: A parked write survives restart and completes on the far side of it
    When I compose an event of kind 1 saying "episode 21 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the write reports accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    And the NIP-46 signing provider for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd" becomes available
    Then the published event is authored by "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received it

  # nmp:id=IDENTITY-NIP46-RECOVERY-003
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1169
  Scenario: A provider for a different key does not unpark it
    When I compose an event of kind 1 saying "episode 22 is up" and publish it naming identity "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And the NIP-46 signing provider for "81b637d8fcd2c6da6359e6963113a1170de795e4b725b84d1e0b4cfd9ec58ce9" becomes available
    Then the write is still awaiting a signer for "f62a697de0475d83990780a93267ba3113dcc90a84047574aeb274837df600fd"
    And "wss://hub.example" received nothing
