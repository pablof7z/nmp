Feature: Explicit write routes stay verbatim for their whole lifetime
  An explicit route is the app's exact destination set. Learning more about
  the author later cannot turn it into an automatically resolved route.

  This is the governed, built counterpart of the `@designed` scenario "A relay
  learned after acceptance is never added to an explicit route" still carried
  in `features/routing/auto-and-explicit.feature`. That file is ungoverned
  legacy corpus; #1074 deliberately excludes bulk legacy annotation, and this
  tool's diff-aware legacy check cannot express deleting one scenario from a
  multi-scenario ungoverned file in the same change that governs it elsewhere.
  Retiring the legacy stub is left to a future full-file governance pass over
  `auto-and-explicit.feature`.

  Background:
    Given I am logged in as my own account

  # nmp:id=ROUTING-AUTOANDEXPLICIT-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::accepted_explicit_route_ignores_later_directory_fact_across_restart
  # nmp:falsifier=Decode the persisted explicit strategy as Auto; recovery appends the later author outbox and the one-destination witness fails.
  @ledger-6
  Scenario: A relay learned after acceptance is never added to an explicit route
    # There is no widen path anywhere: no operation adds a relay to an
    # accepted Explicit route, which is guarantee #6's `NarrowOnly` discipline
    # carried over structurally rather than by convention. Learning more must
    # therefore change nothing here, including across a durable restart.
    Given the engine is offline
    When I publish a note saying "for the archive" to exactly "chosen-relay"
    And my relay list changes to name "outbox-c" as a write relay
    And the engine comes back online
    Then the note is delivered to "chosen-relay"
    And "outbox-c" was never contacted
    And the receipt reports exactly one destination
