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

    # nmp:id=ROUTING-COLLAPSE-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::the_merge_rule_fires_on_every_axis
    # nmp:evidence=rust:nmp-router::local_refilter_is_exact_on_the_tag_axis
    # nmp:falsifier=Remove one array-axis union or deliver the widened relay result without exact local refiltering; one of the exhaustive component or delivery proofs fails.
    Scenario: Values under one array component share a relay request
      Given compatible pending filters differ only in one tag value, author, kind, or event id
      When NMP compiles their relay-local cohort
      Then that component is unioned into one relay filter
      And every logical observation still receives only matching events

    # nmp:id=ROUTING-COLLAPSE-002
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::the_rule_never_merges_across_two_tag_names
    # nmp:falsifier=Union values carried by different tag names; the result becomes a conjunction and the refusal proof fails.
    Scenario: Different tag names never merge
      Given one pending filter selects a p tag and another selects a t tag
      When NMP compiles their relay-local cohort
      Then they remain separate relay filters

    # nmp:id=ROUTING-COLLAPSE-003
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::the_rule_never_merges_a_filter_that_carries_a_limit
    # nmp:evidence=rust:nmp-router::limited_identical_except_authors_atoms_each_reach_the_wire
    # nmp:falsifier=Merge two limited filters or a limited filter with an unlimited sibling; one relay-side result cap silently under-fetches at least one logical query.
    Scenario: A relay-side limit prevents structural union
      Given compatible-looking pending filters include a relay-side limit
      When NMP compiles their relay-local cohort
      Then every limited logical query keeps an independent relay request

    # nmp:id=ROUTING-COLLAPSE-004
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::the_rule_never_merges_two_components_at_once
    # nmp:falsifier=Merge filters differing on two array axes; the cross-product overfetch violates the one-component widening contract.
    Scenario: Two differing components never merge
      Given two pending filters differ in both authors and tag values
      When NMP compiles their relay-local cohort
      Then they remain separate relay filters

  Rule: Large and derived sets stay bounded without fan-out

    # nmp:id=ROUTING-COLLAPSE-005
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::tag_axis_stays_within_relay_subscription_limits_once_coalesced
    # nmp:evidence=rust:nmp-router::the_catalog_has_orders_of_magnitude_of_headroom_before_the_limit_returns
    # nmp:falsifier=Emit one request per tag value or truncate after the first full filter; the measured catalog exceeds the relay budget or loses demanded values.
    Scenario: A large value catalog shards instead of truncating or fanning out
      Given 1200 values are pending for one tag on one relay
      When NMP compiles the cohort with a 500-value filter ceiling
      Then every value appears in some relay filter
      And the request count remains within the relay subscription budget

    # nmp:id=ROUTING-COLLAPSE-006
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::a_collapsed_catalog_of_three_hundred_stays_inside_a_budget_of_twenty
    # nmp:falsifier=Plan each catalog value as its own relay request; the 300-value catalog is locally limited by the advertised budget of twenty.
    Scenario: A realistic catalog stays inside an advertised relay budget
      Given 300 compatible values target a relay advertising twenty subscriptions
      When NMP compiles the relay plan
      Then all 300 values are served without local-limit evidence

    # nmp:id=ROUTING-COLLAPSE-007
    # nmp:status=built
    # nmp:evidence=rust:nmp::b_warm_cache_resolves_the_whole_set_in_one_recompile
    # nmp:evidence=rust:nmp::d_never_eosing_relay_still_serves_every_value
    # nmp:falsifier=Compile a warm derived value one at a time or wait for EOSE before admitting already-ingested values; one of the warm-cache or never-EOSE proofs fails.
    Scenario: Derived values collapse from cache without waiting for EOSE
      Given a live query derives its outer tag values from another query
      When several values are already cached or arrive before EOSE
      Then the outer relay demand includes every known value in a collapsed plan

    # nmp:id=ROUTING-COLLAPSE-008
    # nmp:status=built
    # nmp:evidence=rust:nmp::e_values_arriving_across_two_relays_collapse_per_relay_not_per_value
    # nmp:falsifier=Coalesce before routing or fan out after routing; the two-source proof either crosses relay boundaries or emits one request per value.
    Scenario: Derived value grouping is relay-local
      Given values are discovered through two independent relay sessions
      When their outer demand targets a host relay
      Then NMP groups compatible outer work per target relay session
      And it never groups work across session boundaries
