Feature: A cold engine discovers an author's content route
  The app supplies discovery sources, not author routes. NMP first asks an
  independently configured source for the author's NIP-65 relay list, so the
  relay-list request itself needs no pre-existing author route. It installs
  the resulting neutral route and recompiles the already-live content query
  without turning the discovery source into a generic content fallback.

  Rule: A discovered route precedes content acquisition

    @acceptance
    Scenario: A cold public engine learns the route before fetching content
      Given Alice's note exists only at her content relay
      And Alice's relay list will be published only by the configured indexer
      When a cold public engine observes Alice's notes
      Then the indexer was asked for Alice's relay list
      And the content relay was not contacted before that relay list arrived
      And Alice's content was fetched from her discovered relay
      And the indexer was never used as a generic content fallback
