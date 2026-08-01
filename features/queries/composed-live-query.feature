Feature: One live query can watch several sources at once
  Some correct reads need more than one complete question asked at the same
  time -- the same listing at two different hosts, one selection under two
  different freshness policies -- whose answers form ONE result the app reads.
  Each such question is a branch. A branch is complete on its own: it owns its
  selection, which relays may answer it, the access context it asks under, what
  it will project from cache, and how fresh it insists on being.

  An app declares all of its branches in one live query and observes that query
  once. It gets one handle, one stream of frames, one merged row set, and one
  piece of evidence per branch. It never merges branch results itself, and NMP
  never lets one branch's answers leak into another's question.

  Background:
    Given I am logged in as my own account
    And relay "a" and relay "b" are independent hosts I can watch directly

  Rule: Each branch keeps its own question and its own answer

    # nmp:id=QUERIES-COMPOSED-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::branch_sources_are_never_flattened_into_one_pinned_set
    # nmp:falsifier=Merge every branch's declared relay set into one branch before planning; the query then reports one evidence entry naming both hosts instead of one entry per host naming only its own.
    Scenario: Two branches pinned to two hosts ask each host on its own behalf
      Given one branch asks relay "a" for a listing
      And a second branch asks relay "b" for the same listing
      When I open the query once
      Then relay "a" and relay "b" are each asked
      And the query reports two separate pieces of evidence
      And each names only the host its own branch declared

    # nmp:id=QUERIES-COMPOSED-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::equal_branches_keep_independent_evidence_entries
    # nmp:evidence=rust:nmp-grammar::policy_distinct_branches_are_not_collapsed
    # nmp:falsifier=Treat two branches with equal acquisition identity as one duplicate; one of the two declared policies disappears and only one evidence entry is reported.
    Scenario: Two branches asking the identical question under different freshness stay two branches
      Given one branch asks relay "a" for a listing and insists on going to the relay
      And a second branch asks relay "a" for exactly the same listing but from cache only
      When I open the query once
      Then the query still has two branches
      And each reports its own evidence for the policy it declared

    # nmp:id=QUERIES-COMPOSED-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::an_unplannable_branch_reports_its_own_shortfall
    # nmp:evidence=rust:nmp::a_shortfall_only_reaches_the_branch_that_has_it
    # nmp:falsifier=Report one shortfall list for the whole query; the working branch then inherits its sibling's failure and the two are no longer distinguishable.
    Scenario: A branch nobody can answer reports its own shortfall and spoils nothing else
      Given one branch asks relay "a" for a listing
      And a second branch chases an author whose relays nothing knows
      When I open the query once
      Then the second branch reports explicitly that it fell short
      And the first branch reports no shortfall at all
      And the first branch still reports relay "a" as a source it planned
      But the query never claims to be complete or authoritatively empty as a whole

    # nmp:id=QUERIES-COMPOSED-004
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::a_single_query_is_one_branch_with_no_aggregate_bound
    # nmp:evidence=rust:nmp::cancelling_a_union_keeps_work_a_sibling_observation_still_owns
    # nmp:falsifier=Give a one-branch query a different evidence shape -- no entries, or one query-level rollup; the surviving one-branch observation's single entry naming its own relay stops holding.
    Scenario: A query with one branch is the same kind of query, just smaller
      Given a query with exactly one branch asking relay "a"
      When I open it
      Then it reports exactly one piece of evidence
      And that evidence names relay "a"

    # nmp:id=QUERIES-COMPOSED-005
    # nmp:status=built
    # nmp:evidence=rust:nmp::only_the_branch_tells_two_identical_resolver_facts_apart
    # nmp:falsifier=Drop the branch from an execution fact, or fix it at the first branch; the two traces become indistinguishable and the app can no longer say which branch it is watching.
    Scenario: I can always tell which branch a diagnostic fact came from
      Given two branches ask two different hosts for exactly the same listing
      When I open the query once
      Then both branches produce a resolution fact that is identical in every readable field
      And the branch each fact belongs to is the only thing that distinguishes them
      And the facts are numbered in one sequence for the whole query, not one per branch

  Rule: The query delivers one result, never one result per branch

    # nmp:id=QUERIES-COMPOSED-006
    # nmp:status=built
    # nmp:evidence=rust:nmp::rows_union_by_event_id_with_merged_provenance
    # nmp:falsifier=Deliver one frame per branch and let the app merge them; the observation then sees two frames and the shared event arrives twice, once per branch.
    Scenario: One event served by two branches is one row naming both relays
      Given relay "a" holds an event only it has
      And relay "b" holds an event only it has
      And both relays hold a third event
      When I open a query with one branch per host
      Then I receive one frame, not one per branch
      And I see three rows
      And the shared row names both relay "a" and relay "b"

  Rule: How many rows I get is decided across the whole query

    # nmp:id=QUERIES-COMPOSED-007
    # nmp:status=built
    # nmp:evidence=rust:nmp::the_aggregate_bound_is_applied_after_the_union
    # nmp:falsifier=Apply the cap to each branch before merging; two branches with a cap of two deliver four rows, and an older row that should have lost wins its branch's slot.
    Scenario: A cap of two over two branches means two rows in total
      Given relay "a" holds two of the newest events and relay "b" holds one
      And relay "b" also holds a strictly older event
      When I open a query over both hosts capped at two rows
      Then I receive exactly two rows, not two per branch
      And they are the two newest across both hosts
      And the older event is not among them

    # nmp:id=QUERIES-COMPOSED-008
    # nmp:status=built
    # nmp:evidence=rust:nmp::a_window_bounds_the_union_globally
    # nmp:falsifier=Give each branch its own window target; an initial window of two over two branches holds four rows.
    Scenario: A window of two over two branches holds the two newest rows overall
      Given relay "a" holds three events and relay "b" holds three newer events
      When I open a window of two rows over a query with one branch per host
      Then the window holds two rows
      And they are the two newest across both hosts
      And the window still reports evidence for each branch separately

    # nmp:id=QUERIES-COMPOSED-009
    # nmp:status=built
    # nmp:evidence=rust:nmp::a_window_and_an_aggregate_bound_are_two_owners_of_row_membership
    # nmp:falsifier=Accept both; two independent things then decide how many rows the app holds and growing the window silently fights the cap.
    Scenario: A window and a row cap may not both decide how many rows I hold
      Given a query over two hosts that already caps its result at three rows
      When I try to open it with a growable window as well
      Then opening is refused before anything is watched
      And the refusal says the window and the cap compete for the same count

    # nmp:id=QUERIES-COMPOSED-010
    # nmp:status=built
    # nmp:evidence=rust:nmp::windowed_observe_rejects_bad_bounds_and_competing_limit
    # nmp:falsifier=Allow a window over a branch that carries its own limit; the branch's own limit and the window then both truncate the same rows and growing the window cannot recover what the branch never asked for.
    Scenario: A window is refused when a branch already limits its own results
      Given a branch that asks its host for at most three events
      When I try to open a window over a query containing that branch
      Then opening is refused before anything is watched
      And the refusal says the branch's own limit competes with the window

  Rule: A declaration that could never be observed honestly is refused whole

    # nmp:id=QUERIES-COMPOSED-011
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::every_unconstructible_declaration_is_a_typed_refusal
    # nmp:evidence=rust:nmp::an_over_cap_union_refuses_the_whole_declaration
    # nmp:falsifier=Accept any of these three instead of refusing; an empty query has nothing to report evidence about, a cap of zero can never hold a row, and an inner cap has no scope left to bound so it is silently discarded.
    Scenario Outline: Declaring the unobservable fails at declaration time
      When I declare <declaration>
      Then it is refused with <refusal>
      And no query, handle or relay request is created

      Examples:
        | declaration                                    | refusal                          |
        | a query with no branches at all                | an empty query union             |
        | a query capped at zero rows                    | a zero result cap                |
        | a cap on a branch inside another query         | a nested result cap              |

    # nmp:id=QUERIES-COMPOSED-012
    # nmp:status=built
    # nmp:evidence=rust:nmp::an_over_cap_union_refuses_the_whole_declaration
    # nmp:falsifier=Truncate an over-ceiling declaration to the ceiling instead of refusing; the app then watches a silently smaller query than it declared and no evidence says which branches were dropped.
    Scenario: More branches than the ceiling refuses everything, never a subset
      When I declare one branch more than the supported ceiling
      Then it is refused, naming both how many I asked for and the maximum
      And no subset of my branches is installed

  Rule: The declaration is a set of branches, not the list I happened to type

    # nmp:id=QUERIES-COMPOSED-013
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::permutations_nesting_and_duplicates_share_one_value_and_hash
    # nmp:falsifier=Keep insertion order as identity; the same three branches typed in another order become a different query, so an app that re-declares them reopens work NMP already had, and the evidence order it indexes by shifts underneath it.
    Scenario: The same branches typed in any order are the same query
      Given three branches
      When I declare them in one order, and again in another order with one repeated and one arrived at by combining two smaller queries
      Then both declarations are the same query
      And both list their branches in the same order
      And the repeated branch appears once

    # nmp:id=QUERIES-COMPOSED-014
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::equal_selections_with_different_source_or_access_remain_distinct
    # nmp:falsifier=Key branches on the selection alone; a public branch and a pinned or differently-authenticated branch collapse into one and one of them borrows the other's evidence.
    Scenario: The same selection asked of different sources is a different branch
      Given one branch asks for a selection wherever its authors publish
      And another asks for exactly the same selection pinned to relay "a"
      When I declare both in one query
      Then they remain two branches

    # nmp:id=QUERIES-COMPOSED-015
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::the_aggregate_bound_participates_in_value_identity
    # nmp:falsifier=Exclude the cap from the query's identity; two queries over the same branches with different caps become the same query, so one of the caps is silently ignored.
    Scenario: Changing the row cap declares a different query
      Given the same branches capped at three rows, capped at five rows, and uncapped
      Then all three are different queries

  Rule: Change and teardown move the whole query at once

    # nmp:id=QUERIES-COMPOSED-016
    # nmp:status=built
    # nmp:evidence=rust:nmp::a_reactive_change_moves_every_branch_in_one_frame
    # nmp:falsifier=Emit one frame per affected branch; the app is handed a frame in which one branch has already followed the new account and the other is still showing the old one.
    Scenario: Switching accounts moves every branch in one frame
      Given a query whose branches both follow whoever is logged in
      When I switch to another account
      Then I receive exactly one frame for that change
      And it carries both branches' evidence
      And I am never shown a mixture of the old and new account

    # nmp:id=QUERIES-COMPOSED-017
    # nmp:status=built
    # nmp:evidence=rust:nmp::cancelling_a_union_keeps_work_a_sibling_observation_still_owns
    # nmp:falsifier=Withdraw every branch's work unconditionally on cancel; the unrelated query loses the relay subscription it still owns.
    Scenario: Cancelling gives back only the work nothing else still needs
      Given a query watching relay "a" and relay "b"
      And a separate, unrelated query that also watches relay "a"
      When I cancel the first query
      Then relay "b" is released
      But relay "a" stays live for the unrelated query
      And the unrelated query still reports relay "a" as its own planned source
      And the cancelled query receives no further frames
