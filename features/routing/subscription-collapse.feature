Feature: Compatible relay demand collapses before admission
  Apps may express many independent logical queries while a relay accepts only
  a bounded number of subscriptions. NMP may union compatible array-valued
  filter components before a pending cohort reaches the wire, then locally
  refilter the relay's superset back into each app observation.

  Structural union is deliberately narrow. Exactly one array component may
  differ; scalar windows must match; filters carrying a limit stay separate;
  and a configured value ceiling shards a large union without truncating it.
  Routing happens first, so coalescing is scoped to one relay session.

  Rule: Structural union preserves logical query meaning

    Scenario: Values under one array component share a relay request
      Given compatible pending filters differ only in one tag value, author, kind, or event id
      When NMP compiles their relay-local cohort
      Then that component is unioned into one relay filter
      And every logical observation still receives only matching events

    Scenario: Different tag names never merge
      Given one pending filter selects a p tag and another selects a t tag
      When NMP compiles their relay-local cohort
      Then they remain separate relay filters

    Scenario: A relay-side limit prevents structural union
      Given compatible-looking pending filters include a relay-side limit
      When NMP compiles their relay-local cohort
      Then every limited logical query keeps an independent relay request

    Scenario: Two differing components never merge
      Given two pending filters differ in both authors and tag values
      When NMP compiles their relay-local cohort
      Then they remain separate relay filters

  Rule: Large and derived sets stay bounded without fan-out

    Scenario: A large value catalog shards instead of truncating or fanning out
      Given 1200 values are pending for one tag on one relay
      When NMP compiles the cohort with a 500-value filter ceiling
      Then every value appears in some relay filter
      And the request count remains within the relay subscription budget

    Scenario: A realistic catalog stays inside an advertised relay budget
      Given 300 compatible values target a relay advertising twenty subscriptions
      When NMP compiles the relay plan
      Then all 300 values are served without local-limit evidence

    Scenario: Derived values collapse from cache without waiting for EOSE
      Given a live query derives its outer tag values from another query
      When several values are already cached or arrive before EOSE
      Then the outer relay demand includes every known value in a collapsed plan

    Scenario: Derived value grouping is relay-local
      Given values are discovered through two independent relay sessions
      When their outer demand targets a host relay
      Then NMP groups compatible outer work per target relay session
      And it never groups work across session boundaries
