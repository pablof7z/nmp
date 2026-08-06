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

    # nmp:id=QUERIES-COMPOSED-028
    # nmp:status=built
    # nmp:evidence=rust:nmp::opening_execution_facts_ride_out_on_the_opening_frame
    # nmp:evidence=rust:nmp::every_opening_frame_reports_one_evidence_entry_per_branch
    # nmp:evidence=kotlin:NMPKotlin::branchCountMatchesDeliveredEvidenceCount
    # nmp:evidence=swift:NMP::testBranchCountMatchesDeliveredEvidenceCount
    # nmp:falsifier=Deliver the opening's resolution facts as a frame of their own before the query has opened. Give that frame no evidence and the first thing the app reads accounts for no branch at all, so the entry it looks up for its second branch is not there; give it the opening's evidence instead and the app is told a source has been proven while holding none of the rows that proof came from, which is what makes the following module report no contact list for an account that has one.
    Scenario: The first frame I read is the opening itself, whole
      Given two branches asking two hosts from cache only
      When I open the query once and read the first frame it delivers
      Then that frame reports one piece of evidence per branch I declared
      And the evidence describes exactly the rows that frame delivered, never a later state
      And the resolution facts the opening produced arrive on it rather than ahead of it

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
    # nmp:evidence=swift:NMP::testDeclarationOrderDoesNotChangeIdentity
    # nmp:evidence=kotlin:NMPKotlin::declarationOrderDoesNotChangeIdentity
    # nmp:evidence=swift:NMP::testNestedInputFlattensIntoOneCanonicalSet
    # nmp:evidence=kotlin:NMPKotlin::nestedInputFlattensIntoOneCanonicalSet
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

  Rule: Faults and restarts keep the whole query honest

    # nmp:id=QUERIES-COMPOSED-018
    # nmp:status=built
    # nmp:evidence=rust:nmp::a_union_branch_that_cannot_open_leaves_no_earlier_branch_installed
    # nmp:evidence=rust:nmp::a_union_branch_whose_graph_fails_withdraws_the_branches_opened_before_it
    # nmp:falsifier=Keep the branches that were already opened when a later one fails; the refused query's first branch keeps its demand atom and is handed a relay request on the very next recompile, and the branch abandoned before any handle existed keeps its graph nodes.
    Scenario: A branch that cannot be opened leaves nothing behind
      Given a query whose second branch cannot read its initial local view
      When I try to open it
      Then opening is refused without a handle
      And the first branch's relay request and reserved work are released too

    # nmp:id=QUERIES-COMPOSED-019
    # nmp:status=built
    # nmp:evidence=rust:nmp::one_branchs_refresh_failure_retracts_no_sibling_row
    # nmp:falsifier=Refresh from whichever branches could be read and drop the one that failed; the branch nothing could be read about has all of its rows reported as removed, so the app watches half its list blink out because one source hiccuped.
    Scenario: One branch failing to refresh never retracts another branch's rows
      Given a live query over two hosts that has already delivered rows
      When one branch's local read fails while the query refreshes
      Then I keep every row and every piece of evidence I already had
      And no row is reported as removed
      And the failure is reported as a diagnostic instead

    # nmp:id=QUERIES-COMPOSED-020
    # nmp:status=built
    # nmp:evidence=rust:nmp::each_redeclared_branch_decides_freshness_from_its_own_stored_coverage
    # nmp:evidence=rust:nmp::a_redeclared_window_starts_again_at_its_initial_size
    # nmp:falsifier=Make one freshness decision for the whole query and give it to every branch, and let anything carry a window's grown target across the restart; a branch whose own host was never reconciled then rides on a sibling's stored coverage and is never asked, and the redeclared window opens at the size the previous observation had grown to.
    Scenario: Redeclaring the query after a restart starts each branch afresh
      Given a query over two hosts whose window I had already grown
      When the app restarts and declares exactly the same query again
      Then each branch decides for itself whether it needs the relay, from its own stored coverage
      And the window starts again at its initial size
      And nothing durable had been kept that continues the previous observation

  Rule: The query I am holding is the same query NMP is holding

    # nmp:id=QUERIES-COMPOSED-021
    # nmp:status=built
    # nmp:evidence=swift:NMP::testBranchCountMatchesDeliveredEvidenceCount
    # nmp:evidence=kotlin:NMPKotlin::branchCountMatchesDeliveredEvidenceCount
    # nmp:evidence=rust:nmp-grammar::permutations_nesting_and_duplicates_share_one_value_and_hash
    # nmp:falsifier=Leave the branches I typed in the query I am holding and collapse the repeat somewhere deeper instead; I read three branches off my own declaration, three rows of a branch list appear in my interface, two pieces of evidence arrive, and every piece I line up against my list past the repeat describes a different host than the one I have drawn it next to.
    Scenario: The branches I can read back are exactly the ones evidence is reported for
      Given I declare one query asking relay "a", relay "b", and relay "a" again
      When I read my own declaration back
      Then it has two branches
      And opening it delivers exactly two pieces of evidence, one for each of them

    # nmp:id=QUERIES-COMPOSED-022
    # nmp:status=built
    # nmp:evidence=rust:nmp::per_branch_evidence_is_indexed_by_canonical_branch_order
    # nmp:evidence=swift:NMP::testDeclarationOrderDoesNotChangeIdentity
    # nmp:evidence=kotlin:NMPKotlin::declarationOrderDoesNotChangeIdentity
    # nmp:falsifier=Order the branches a second time anywhere after the query has decided what it is -- a per-platform re-implementation of the composing rule, or the order the observation assembles its branches in -- so that the order I read and the order evidence arrives in can drift apart; the two hosts' entries swap, and I read relay "b"'s sources, its shortfall and its diagnostics as relay "a"'s, with no count, no total and no other reading disagreeing.
    Scenario: Which branch a piece of evidence belongs to does not depend on how I typed the query
      Given one branch asking relay "a" and one branch asking relay "b"
      When I declare them in one order, and again in the other, and open both
      Then both declarations list their branches in the same order
      And in each, the evidence in a given position names the host the branch in that position asked

    # nmp:id=QUERIES-COMPOSED-023
    # nmp:status=built
    # nmp:evidence=swift:NMP::testDeclarationOrderDoesNotChangeIdentity
    # nmp:evidence=kotlin:NMPKotlin::declarationOrderDoesNotChangeIdentity
    # nmp:evidence=rust:nmp-grammar::permutations_nesting_and_duplicates_share_one_value_and_hash
    # nmp:falsifier=Decide sameness from the canonical branches but derive the lookup key from the branches as they were typed; the two declarations compare equal and still file under different keys, so my table of what I am already watching misses, I open a second observation of a query NMP considers unchanged, and the first one stays live with nothing left holding it.
    Scenario: The same query re-declared in another order is the same lookup key, not merely an equal value
      Given a query of two branches that I have stored in a table of what I am already watching
      When I declare the same two branches in the other order
      Then the two compare equal
      And they hash the same, so the table finds the entry I stored under either declaration

    # nmp:id=QUERIES-COMPOSED-024
    # nmp:status=built
    # nmp:evidence=swift:NMP::testEveryRefusalIsItsOwnTypedError
    # nmp:evidence=kotlin:NMPKotlin::everyRefusalIsItsOwnTypedError
    # nmp:evidence=rust:nmp-grammar::every_unconstructible_declaration_is_a_typed_refusal
    # nmp:falsifier=Take the declaration as written and refuse only when it is handed over to be watched; I build my queries at startup and store one I can never open, I learn its cap is zero only when a screen tries to show it, and a declaration that already passed every refusal can be emptied again afterwards while nothing rechecks it.
    Scenario: An unobservable declaration is refused where I write it, not where I watch it
      When I declare a query with no branches at all
      Then it is refused as I write it, with no engine, no account and no relay involved
      And the same holds for a cap of zero, for a cap on a branch inside another query, and for more branches than the ceiling

    # nmp:id=QUERIES-COMPOSED-025
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::the_branch_ceiling_counts_the_canonical_set_not_the_input_list
    # nmp:falsifier=Count the branches I handed in rather than the branches that survive; two queries I compose that overlap are refused for exceeding a ceiling their combined branches never reach, and the number in the refusal counts branches the query would never have opened.
    Scenario: The ceiling counts the branches the query ends up with, not the ones I typed
      Given as many distinct branches as the supported ceiling allows
      When I compose them all with one of them named a second time
      Then the query is accepted
      And it has exactly the ceiling number of branches

    # nmp:id=QUERIES-COMPOSED-026
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::a_single_query_is_one_branch_with_no_aggregate_bound
    # nmp:evidence=swift:NMP::testDuplicateBranchAppearsOnce
    # nmp:evidence=kotlin:NMPKotlin::duplicateBranchAppearsOnce
    # nmp:falsifier=Build the one-branch declaration the same way several branches are composed, and hand back the failure that way can produce; the one declaration that can violate nothing now returns an error with no cause, every call site is written as though it always succeeds, and the first new refusal composing ever gains becomes a crash in code that never had a reason to handle one.
    Scenario: Declaring a single branch cannot fail, and composing it alone is the same query
      Given one branch
      When I declare a query of just that branch
      Then there is no refusal for me to handle
      And composing that query with nothing but itself gives back the same query

    # nmp:id=QUERIES-COMPOSED-027
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1215
    Scenario: A query that was accepted can never turn into one that would be refused
      Given a query I declared and that was not refused
      Then it still has every branch it was accepted with, whatever my code does with it afterwards
      And neither I nor a library I hand it to can leave it with no branches at all
