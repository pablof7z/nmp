Feature: Neutral author routes never turn discovery sources into content fallbacks
  The fixture supplies current neutral author routes for ordinary read
  routing. Operator NIP-65 sources remain protocol-only: generic content is
  fetched from the authors' outbound relays, never from those sources.

  Scenario: Content is fetched from the author's own write relay
    Given only 2 indexer relays are configured
    And Alice's relay list names "alice-relay" as her write relay
    And Alice has posted a note saying "hello from alice, over her own relay"
    And I am logged in as an account that follows Alice
    When I open a feed of my follows' notes
    Then Alice's notes arrive from "alice-relay"
    And no relay outside the indexers, "me-relay", and "alice-relay" was ever contacted
