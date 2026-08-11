Feature: NIP-65 sources are not generic content routes
  Operator-selected discovery sources belong only to the optional protocol
  coordinator. They do not become neutral author routes, app relays, or
  fallback relays merely because the same engine is fetching content.

  Rule: Configuration keeps protocol acquisition separate from content routing

    # nmp:id=ROUTING-SELFBOOTSTRAPPINGOUTBOX-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::indexer_relays_are_not_generic_routing_facts
    # nmp:falsifier=Copy indexer_relays into operator app or fallback routing facts; the owner test fails.
    Scenario: Discovery sources never become content fallbacks
      Given an operator configures a source for NIP-65 relay lists
      When generic content routing is assembled
      Then that source is absent from every generic routing fact
      And no author route is fabricated from configuration
