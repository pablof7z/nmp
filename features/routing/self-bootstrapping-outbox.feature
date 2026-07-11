Feature: The engine finds everyone's relays on its own
  Given nothing but two indexer relays, the engine discovers where each
  followed author actually writes and fetches content there -- the app
  resolves no relays at all; there is nowhere to even pass one in.

  Scenario: Content is fetched from the author's own write relay
    Given only 2 indexer relays are configured
    And Alice's relay list names "alice-relay" as her write relay
    And Alice has posted a note saying "hello from alice, over her own relay"
    And I am logged in as an account that follows Alice
    When I open a feed of my follows' notes
    Then the indexers are asked only for relay lists and profiles
    And Alice's notes arrive from "alice-relay"
    And no relay outside the indexers and "alice-relay" was ever contacted
