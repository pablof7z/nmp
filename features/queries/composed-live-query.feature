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

    Scenario: Two branches pinned to two hosts ask each host on its own behalf
      Given one branch asks relay "a" for a listing
      And a second branch asks relay "b" for the same listing
      When I open the query once
      Then relay "a" and relay "b" are each asked
      And the query reports two separate pieces of evidence
      And each names only the host its own branch declared

    Scenario: Two branches asking the identical question under different freshness stay two branches
      Given one branch asks relay "a" for a listing and insists on going to the relay
      And a second branch asks relay "a" for exactly the same listing but from cache only
      When I open the query once
      Then the query still has two branches
      And each reports its own evidence for the policy it declared

    Scenario: A branch nobody can answer reports its own shortfall and spoils nothing else
      Given one branch asks relay "a" for a listing
      And a second branch chases an author whose relays nothing knows
      When I open the query once
      Then the second branch reports explicitly that it fell short
      And the first branch reports no shortfall at all
      And the first branch still reports relay "a" as a source it planned
      But the query never claims to be complete or authoritatively empty as a whole

    Scenario: A query with one branch is the same kind of query, just smaller
      Given a query with exactly one branch asking relay "a"
      When I open it
      Then it reports exactly one piece of evidence
      And that evidence names relay "a"

    Scenario: I can always tell which branch a diagnostic fact came from
      Given two branches ask two different hosts for exactly the same listing
      When I open the query once
      Then both branches produce a resolution fact that is identical in every readable field
      And the branch each fact belongs to is the only thing that distinguishes them
      And the facts are numbered in one sequence for the whole query, not one per branch

    Scenario: The first frame I read is the opening itself, whole
      Given two branches asking two hosts from cache only
      When I open the query once and read the first frame it delivers
      Then that frame reports one piece of evidence per branch I declared
      And the evidence describes exactly the rows that frame delivered, never a later state
      And the resolution facts the opening produced arrive on it rather than ahead of it

  Rule: The query delivers one result, never one result per branch

    Scenario: One event served by two branches is one row naming both relays
      Given relay "a" holds an event only it has
      And relay "b" holds an event only it has
      And both relays hold a third event
      When I open a query with one branch per host
      Then I receive one frame, not one per branch
      And I see three rows
      And the shared row names both relay "a" and relay "b"

  Rule: How many rows I get is decided across the whole query

    Scenario: A cap of two over two branches means two rows in total
      Given relay "a" holds two of the newest events and relay "b" holds one
      And relay "b" also holds a strictly older event
      When I open a query over both hosts capped at two rows
      Then I receive exactly two rows, not two per branch
      And they are the two newest across both hosts
      And the older event is not among them

    Scenario: A window of two over two branches holds the two newest rows overall
      Given relay "a" holds three events and relay "b" holds three newer events
      When I open a window of two rows over a query with one branch per host
      Then the window holds two rows
      And they are the two newest across both hosts
      And the window still reports evidence for each branch separately

    Scenario: A window and a row cap may not both decide how many rows I hold
      Given a query over two hosts that already caps its result at three rows
      When I try to open it with a growable window as well
      Then opening is refused before anything is watched
      And the refusal says the window and the cap compete for the same count

    Scenario: A window is refused when a branch already limits its own results
      Given a branch that asks its host for at most three events
      When I try to open a window over a query containing that branch
      Then opening is refused before anything is watched
      And the refusal says the branch's own limit competes with the window

  Rule: A declaration that could never be observed honestly is refused whole

    Scenario Outline: Declaring the unobservable fails at declaration time
      When I declare <declaration>
      Then it is refused with <refusal>
      And no query, handle or relay request is created

      Examples:
        | declaration                                    | refusal                          |
        | a query with no branches at all                | an empty query union             |
        | a query capped at zero rows                    | a zero result cap                |
        | a cap on a branch inside another query         | a nested result cap              |

    Scenario: More branches than the ceiling refuses everything, never a subset
      When I declare one branch more than the supported ceiling
      Then it is refused, naming both how many I asked for and the maximum
      And no subset of my branches is installed

  Rule: The declaration is a set of branches, not the list I happened to type

    Scenario: The same branches typed in any order are the same query
      Given three branches
      When I declare them in one order, and again in another order with one repeated and one arrived at by combining two smaller queries
      Then both declarations are the same query
      And both list their branches in the same order
      And the repeated branch appears once

    Scenario: The same selection asked of different sources is a different branch
      Given one branch asks for a selection wherever its authors publish
      And another asks for exactly the same selection pinned to relay "a"
      When I declare both in one query
      Then they remain two branches

    Scenario: Changing the row cap declares a different query
      Given the same branches capped at three rows, capped at five rows, and uncapped
      Then all three are different queries

  Rule: Change and teardown move the whole query at once

    Scenario: Switching accounts moves every branch in one frame
      Given a query whose branches both follow whoever is logged in
      When I switch to another account
      Then I receive exactly one frame for that change
      And it carries both branches' evidence
      And I am never shown a mixture of the old and new account

    Scenario: Cancelling gives back only the work nothing else still needs
      Given a query watching relay "a" and relay "b"
      And a separate, unrelated query that also watches relay "a"
      When I cancel the first query
      Then relay "b" is released
      But relay "a" stays live for the unrelated query
      And the unrelated query still reports relay "a" as its own planned source
      And the cancelled query receives no further frames

  Rule: Faults and restarts keep the whole query honest

    Scenario: A branch that cannot be opened leaves nothing behind
      Given a query whose second branch cannot read its initial local view
      When I try to open it
      Then opening is refused without a handle
      And the first branch's relay request and reserved work are released too

    Scenario: One branch failing to refresh never retracts another branch's rows
      Given a live query over two hosts that has already delivered rows
      When one branch's local read fails while the query refreshes
      Then I keep every row and every piece of evidence I already had
      And no row is reported as removed
      And the failure is reported as a diagnostic instead

    Scenario: Redeclaring the query after a restart starts each branch afresh
      Given a query over two hosts whose window I had already grown
      When the app restarts and declares exactly the same query again
      Then each branch decides for itself whether it needs the relay, from its own stored coverage
      And the window starts again at its initial size
      And nothing durable had been kept that continues the previous observation

  Rule: The query I am holding is the same query NMP is holding

    Scenario: The branches I can read back are exactly the ones evidence is reported for
      Given I declare one query asking relay "a", relay "b", and relay "a" again
      When I read my own declaration back
      Then it has two branches
      And opening it delivers exactly two pieces of evidence, one for each of them

    Scenario: Which branch a piece of evidence belongs to does not depend on how I typed the query
      Given one branch asking relay "a" and one branch asking relay "b"
      When I declare them in one order, and again in the other, and open both
      Then both declarations list their branches in the same order
      And in each, the evidence in a given position names the host the branch in that position asked

    Scenario: The same query re-declared in another order is the same lookup key, not merely an equal value
      Given a query of two branches that I have stored in a table of what I am already watching
      When I declare the same two branches in the other order
      Then the two compare equal
      And they hash the same, so the table finds the entry I stored under either declaration

    Scenario: An unobservable declaration is refused where I write it, not where I watch it
      When I declare a query with no branches at all
      Then it is refused as I write it, with no engine, no account and no relay involved
      And the same holds for a cap of zero, for a cap on a branch inside another query, and for more branches than the ceiling

    Scenario: The ceiling counts the branches the query ends up with, not the ones I typed
      Given as many distinct branches as the supported ceiling allows
      When I compose them all with one of them named a second time
      Then the query is accepted
      And it has exactly the ceiling number of branches

    Scenario: Declaring a single branch cannot fail, and composing it alone is the same query
      Given one branch
      When I declare a query of just that branch
      Then there is no refusal for me to handle
      And composing that query with nothing but itself gives back the same query

    Scenario: A query that was accepted can never turn into one that would be refused
      Given a query I declared and that was not refused
      Then it still has every branch it was accepted with, whatever my code does with it afterwards
      And neither I nor a library I hand it to can leave it with no branches at all
