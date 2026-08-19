Feature: Every nested query keeps its own cache and freshness policy
  A derived value comes from an independently declared query. Its local-row
  eligibility and remote acquisition policy are not inherited from, or
  overwritten by, the outer query that consumes the value.

  Rule: Strict cache projection uses the nested query's own pinned sources

    Scenario: A row seen only outside the pinned source contributes no derived value
      Given a cached row matches a nested query
      And that row was observed only from relay B
      And the nested query is pinned to relay A with strict cache projection
      When an app observes the outer live query
      Then the B-only row contributes no derived value
      And no outer result or remote request is created from that row

    Scenario: The same row remains eligible for an agnostic nested query
      Given a cached row matches a nested query
      And that row was observed only from relay B
      And the nested query is pinned to relay A with agnostic cache projection
      When an app observes the outer live query
      Then the row contributes its derived value

    Scenario: A row observed by the pinned source remains eligible
      Given a cached row matches a nested query
      And that row was observed from both relay A and relay B
      And the nested query is pinned to relay A with strict cache projection
      When an app observes the outer live query
      Then the row contributes its derived value

    Scenario: Strict provenance is applied before the nested result limit
      Given the newest cached match was observed only from relay B
      And an older cached match was observed from relay A
      And the nested query is pinned to relay A with strict cache projection
      And the nested query requests only its newest eligible row
      When an app observes the outer live query
      Then the older A-observed row contributes its derived value
      And the newer B-only row does not consume the result limit

  Rule: Every query boundary decides its own remote acquisition

    Scenario: A cache-only nested query opens no remote work under a live outer query
      Given an outer query is live
      And its nested query is cache-only
      When an app opens the outer query
      Then the nested query creates no remote request
      And the outer query's own live request is still created

    Scenario: A live nested query remains live under a cache-only outer query
      Given an outer query is cache-only
      And its nested query is live
      When an app opens the outer query
      Then the outer query creates no remote request
      And the nested query's own live request is created

    Scenario: A max-age nested query uses only its own scoped coverage
      Given an outer query and its nested query have different planned sources
      And the nested query requires recent coverage
      When only the nested query's complete planned sources have recent coverage
      Then the nested query creates no remote request
      And the outer query makes its own unchanged freshness decision
      And the snapshot reports the nested query's scoped evidence without a global completion claim

    Scenario: Stale nested coverage degrades only that nested query to live
      Given an outer query and its nested query have different planned sources
      And the nested query requires recent coverage
      When the nested query's coverage is stale or incomplete
      Then the nested query creates its ordinary live remote request
      And the outer query's acquisition and unrelated live queries remain unchanged

    Scenario: Reopening after restart reuses persisted nested coverage
      Given a nested max-age query has complete recent scoped coverage
      When the engine is reconstructed from the same durable store
      And an app reopens the outer query
      Then the nested query creates no remote request
      And its persisted source evidence remains scoped to the nested question

    Scenario: Fresh no-wire scope reports coverage satisfaction independently
      Given a max-age query has complete recent scoped coverage
      When it opens alone or beside an identical live sibling
      Then it creates no remote request of its own
      And its source status is CoverageSatisfied independently of link state
      And it never borrows the live sibling's request-placement status
