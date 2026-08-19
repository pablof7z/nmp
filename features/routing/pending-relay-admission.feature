Feature: Relay work waits briefly for compatible pending demand
  An app may own thousands of independent observations; it never has to batch,
  shard, or pre-aggregate them for NMP. Each observation keeps its own local
  projection, evidence, and cancellation. Only unsent relay work waits: the
  first uncovered observation opens a 10ms cohort, NMP groups compatible relay
  demand behind that boundary, and the resulting REQs become immutable once
  admitted.

  Rule: A pending cohort delays wire work, never cache delivery

    Scenario: Independent avatar observations cause one grouped relay request
      Given several independently cancellable avatar observations need kind:0 profiles from the same relay
      And each profile observation is unbounded because its demand has no result limit
      When those observations open inside one 10ms admission cohort
      Then every observation receives its own local projection and evidence immediately
      And NMP alone groups their compatible relay demand into one unbounded request
      And later arrivals cannot extend the cohort's first-arrival deadline

  Rule: Sent requests are immutable admission facts

    Scenario: A second admission wave opens another request
      Given an earlier admission wave already sent a running relay request
      When a second admission wave asks for compatible but uncovered demand
      Then the earlier request stays byte-for-byte unchanged
      And the second wave opens an additional request
      And no close is sent for the earlier request

    Scenario Outline: Exact demand attaches to the truthful incumbent request phase
      Given an earlier admission wave has a request covering the exact logical demand that is <phase>
      When another independent observation asks for that demand
      Then it receives its own cached projection immediately
      And NMP emits no relay request and performs no router compile
      And its acquisition evidence reports <current phase>
      And any still-outstanding settlement or close reports only to the exact owners attached when that terminal arrives
      And an already-settled request does not replay historical request or settlement facts

      Examples:
        | phase                              | current phase          |
        | accepted and awaiting a terminal   | stored events streaming |
        | already settled and still live     | stored events finished  |

  Rule: Observation lifecycle work is proportional to the observation changed

    Scenario: Opening and closing many observations never reprojects siblings
      Given many observations already have independent cached projections
      When more ordinary or windowed observations open or close
      Then only each newly opened observation reads its own canonical rows
      And plan changes refresh acquisition evidence only for affected demand
      And closing observations does not reread surviving rows

    Scenario: Independent observations withdraw by exact owner delta
      Given ten thousand independently cancellable observations share bounded demand covered by immutable relay requests
      When each observation withdraws through its own cancellation
      Then each non-final withdrawal touches only its departing exact ownership edge
      And it leaves sibling projections and evidence unchanged
      And it reads no sibling projection or coverage and emits no wire or diagnostics frame
      And the final owner emits exactly one close for each physical request
      And detached exact demand can reattach to a still-running covering request without a new REQ

    Scenario: Final routeless ownership retracts its diagnostic without wire work
      Given one live observation owns outbox demand with no candidate relay
      And diagnostics report its author as uncovered
      When that observation withdraws its final exact ownership
      Then no relay request or close is emitted
      And the uncovered-author diagnostic is removed in the same reducer call

    Scenario: Pre-admission cancellation removes only its pending ownership
      Given distinct compatible observations are pending and no request has been sent
      When one observation cancels before the admission boundary
      Then only its exact pending atom is removed
      And surviving pending demand is neither inspected nor reconstructed
      And no store read, router compile, diagnostic frame, or wire operation occurs

    Scenario: Reattached demand keeps its already-sent request alive
      Given two independent observations share one already-sent immutable request
      And one observation withdraws and later reopens while the sibling still owns that request
      When the sibling withdraws
      Then the already-sent request remains byte-for-byte unchanged and no close is sent
      And the reopened observation remains independently cancellable
      And its final withdrawal emits exactly one close
      And a delayed acceptance for the retired request cannot restore observation or wire ownership

    Scenario: A later cohort touches no incumbent relay ownership
      Given ten thousand admitted relay requests and an earlier refusal remain active
      When one later uncovered observation reaches its admission boundary
      Then NMP compiles only that new cohort
      And no incumbent demand, request, pending owner, or refusal diagnostic is visited
      And every earlier request and refusal remains byte-for-byte unchanged
      And acquisition evidence is refreshed only for the newly covered observation

    Scenario: Departed attribution shapes do not wait for unrelated owners
      Given two independent observations contribute different current pre-EOSE claim shapes
      When one observation withdraws and its exact current request claim ownership ends
      Then its attribution shape is released immediately
      And the unrelated observation and its shape remain active

    Scenario: Current request claims follow exact local ownership before EOSE
      Given one immutable request has a claim shared by two exact local owners
      When one owner withdraws before end of stored events
      Then the remaining alias keeps the claim in the current generation
      When the last exact owner withdraws
      Then the current generation drops that claim without rewriting wire bytes
      And a late EOSE cannot persist the departed claim
      And reattachment before EOSE restores the claim for that current generation

    Scenario: Shared selection keeps each owner's routing facts independently cancellable
      Given independent observations share one exact selection and contribute different routing evidence
      When either observation withdraws first
      Then the effective routing evidence is exactly the union of the owners still active
      And an already-sent request remains byte-for-byte unchanged
      And the final owner releases all routing-evidence ownership

    Scenario: Every logical outbox demand owns its exact shortfall contribution
      Given one author has independent logical outbox demands with different routing outcomes
      And a partially served k=2 demand may own one immutable request and one remaining deficit
      When either exact demand withdraws first
      Then the survivor's requested count, achieved count, and reason equal a fresh compile of that survivor
      And the public author fact reduces simultaneous contributions by greatest deficit and stable reason priority
      And DemandKey or input ordering cannot change that public fact

    Scenario: A later projected hint heals only the missing assignment
      Given one owner supplied one relay hint for a k=2 demand and its immutable request is active
      When another exact owner contributes a second unique relay hint
      Then NMP opens exactly one new request to the missing relay
      And the incumbent request remains byte-for-byte unchanged
      And combined incumbent plus cohort assignment retracts the shortfall
      And withdrawing either owner before or after the pending flush keeps only the live evidence union
      And a duplicate hint performs no compile or wire work

    Scenario: A sent request reports only to the observations it absorbed
      Given many independent observations resolve to current concrete filters
      And some same-filter observations are cache-only while others own relay work
      And same-selection observations with different limits or windows remain distinct relay demand
      And nested Demand boundaries may resolve the same exact relay demand while only one boundary owns wire work
      When NMP sends separate incompatible requests or one compatible grouped request
      Then each request visits every wire-active absorbed observation exactly once
      And no cache-only or unrelated sibling receives a relay-request fact
      And each window-distinct request reports only to its exact logical owner on send and replay
      And each nested request reports only to its wire-participating structural occurrence
      And a NIP-77 candidate, reconciliation, refusal, and fallback retain that same occurrence distinction
      And either close order and a later live reopen preserve that distinction
      And a changed filter revision replaces the earlier target before active relay work reattaches
      And final cancellation releases every execution-evidence owner

    Scenario: Request terminals refresh only exact current owners
      Given many unrelated ordinary observations and histories remain active
      When one ordinary EOSE or correlated NEG completion becomes trustworthy
      Then NMP visits only handles attached to its exact coverage keys and logical demands
      And one handle affected by both dimensions is refreshed only once
      And a bounded or poisoned EOSE records no false coverage while its current owner still reports finished stored events
      And a limit:0 NIP-77 barrier remains nonterminal
      And no sibling coverage read, evidence frame, or eager diagnostics snapshot is produced

    Scenario: Local placement stays awaiting until exact transport acceptance
      Given a connected source has one planned local request that is not yet accepted
      Then its source status is AwaitingRequest before wire dispatch
      When local transport refuses that placement
      Then execution records RequestDeferred and never RelayRefused or Requesting
      And exactly one engine-owned retry and deadline remain
      When the exact retry handoff is accepted
      Then the source status becomes Requesting
      And candidate, reconciliation, and repair role ids report through their plan source
      And withdrawal cancels the attempt or retry ownership

    Scenario: Grouped request evidence does not duplicate the wire filter
      Given many independent observations are served by one grouped relay request
      When the transport accepts that immutable request
      Then every observation receives its own ordered relay-request fact
      And every fact reports the same exact wire filter
      But NMP retains only one immutable filter payload shared by those facts
