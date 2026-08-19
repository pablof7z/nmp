Feature: Fresh and recovered writes share the same cold-start park
  A process restart cannot give an unresolved automatic write a different
  lifecycle from a write accepted while the same author route is unknown.

  This is the governed, built counterpart of the `@designed` scenario "A young
  directory treats a fresh write and a recovered one alike" still carried in
  `features/routing/cold-start-park.feature`. That file is ungoverned legacy
  corpus; #1074 deliberately excludes bulk legacy annotation, and this tool's
  diff-aware legacy check cannot express deleting one scenario from a
  multi-scenario ungoverned file in the same change that governs it elsewhere.
  Retiring the legacy stub is left to a future full-file governance pass over
  `cold-start-park.feature`.

  Background:
    Given I am logged in as my own account

  @ledger-6
  Scenario: A young directory treats a fresh write and a recovered one alike
    # Two writes in the same condition -- one signed just now, one recovered
    # from a store -- must reach the same state, and the same later route
    # fact must release both.
    Given my relay list has never been fetched
    And a note saying "from before" is recovered from the durable store with its routing unresolved
    When I publish a note saying "from now" and let NMP figure out the routing
    Then both receipts report they are still determining destinations
    When my signed relay list arrives naming "outbox-a" as my write relay
    Then both exact notes are delivered to "outbox-a"
