Feature: Respecting what a relay says it can hold
  Relays tell you what they can do. Ask nostr.wine and it says it will hold 50
  concurrent subscriptions; nos.lol and primal say 20; damus says 200. Ask
  nostr.band or snort and they say nothing at all -- no document, no numbers,
  and those are perfectly ordinary relays serving perfectly ordinary clients.

  So there are two obligations here and they pull against each other. A relay
  that names a number must have it respected: opening 40 subscriptions on a
  relay that will hold 20 means half of them fail, and which half is the
  relay's choice, not ours. A relay that names NO number must be left alone:
  inventing a limit for it would drop demand it never refused, and would do so
  again every time some other relay's document failed to load.

  What settles the tension is that whatever a limit removes must be REFUSED
  OUT LOUD -- reported to the app as demand that could not be requested, never
  quietly missing from a plan that still claims to be complete. A relay that
  published nothing therefore loses nothing: the subscription count is
  observable on every relay whether it named a limit or not, so a fan-out
  escaping the merge rules is caught here rather than by taking it out on
  someone's feed.

  Every scenario below reads what NMP actually put on the relay's socket, and
  what the app was actually told. The relay's own document is served over
  plain HTTP on the relay's own address, exactly as a real relay serves it,
  and fetched by the same code that fetches damus's.

  Background:
    Given I am logged in as my own account
    And relay "hub" is the relay I watch directly

  # ---- what a realistic catalog costs ----------------------------------

  Scenario: A catalog of three hundred groups fits inside a limit of twenty
    # The sequencing that makes this whole feature safe to have. Before the
    # merge rules collapsed onto one structural rule, this exact demand
    # compiled to 300 subscriptions per host -- so a limit of 20 would not
    # have been a guard rail, it would have dropped 280 groups' worth of
    # coverage and reported a shortfall for nearly everything I asked about.
    # After the collapse it is ONE subscription carrying all 300 values, and a
    # limit of 20 is 19 subscriptions of headroom.
    Given relay "hub" allows only 20 subscriptions at a time
    And I administer 300 groups
    When I open the group state of every group I administer
    Then relay "hub" is known to allow only 20 subscriptions at a time
    And relay "hub" serves every "d" watch with 1 subscription
    And nothing I asked for was refused for want of a subscription
    And every "d" value I watch is covered by some subscription on relay "hub"

  Scenario Outline: The generous relay and the strict one do the same work
    # A limit is a bound, not a shaping input: it may only ever remove, and
    # for demand that fits it must change nothing at all. damus says 200,
    # primal says 20, and this catalog costs one subscription at either.
    Given relay "hub" allows only <limit> subscriptions at a time
    And I administer 300 groups
    When I open the group state of every group I administer
    # Two subscriptions in total, at either limit: the catalog itself, and the
    # question that resolves it ("which groups am I an admin of"). Neither
    # number changes anything.
    Then relay "hub" serves every "d" watch with 1 subscription
    And relay "hub" is never asked to hold more than <limit> subscriptions
    And nothing I asked for was refused for want of a subscription

    Examples:
      | limit |
      | 20    |
      | 200   |

  Scenario: Ordinary watching stays far inside a strict relay's limit
    # Not the catalog shape -- just several separate things being watched at
    # once, which is what an app actually does. Two tag values and two authors
    # collapse onto two subscriptions (one per axis; the axes are conjunctive
    # and must not merge), nowhere near 20.
    Given relay "hub" allows only 20 subscriptions at a time
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    And I watch for notes from Alice
    And I watch for notes from Bob
    Then relay "hub" serves every "p" watch with 1 subscription
    And relay "hub" serves every author watch with 1 subscription
    And relay "hub" is never asked to hold more than 20 subscriptions
    And nothing I asked for was refused for want of a subscription

  # ---- the relay that says nothing --------------------------------------

  Scenario: A relay that publishes nothing about itself is not held to a guess
    # nostr.band and snort serve no document at all. Every one of these
    # watches carries its own result bound, so none of them can be folded into
    # another -- four separate subscriptions is the honest cost of asking four
    # separate questions. All four must reach the relay. Refusing any of them
    # would mean acting on a limit nobody ever stated.
    Given relay "hub" publishes nothing about itself
    When I watch for the latest 10 notes tagged "p" as "alice"
    And I watch for the latest 10 notes tagged "p" as "bob"
    And I watch for the latest 10 notes tagged "p" as "carol"
    And I watch for the latest 10 notes tagged "p" as "dave"
    # The behavioural assertion is the load-bearing one: all four watches
    # reach the relay. "Nothing is known" is the weaker half deliberately --
    # a relay that answered 404 and a relay whose document has not been asked
    # for yet look identical from here, so it can only witness that no number
    # was invented, not that one was asked for. That the relay really does
    # answer 404 is pinned in the harness's own falsifier
    # (`nmp-test-support`'s `a_scripted_relay_serves_its_nip11_document_over_
    # plain_http`).
    Then nothing is known about how many subscriptions relay "hub" allows
    And relay "hub" is holding 4 subscriptions
    And nothing I asked for was refused for want of a subscription
    And every "p" value I watch is covered by some subscription on relay "hub"

  Scenario: The count is watched even where there is no limit to compare it to
    # What replaces enforcement on a silent relay. If a future change lets an
    # axis escape the merge rules again, three values under one tag name stop
    # being one subscription -- and this notices, on a relay that never named
    # a number. That regression guard, not any individual merge rule, is what
    # keeps the silent relays safe.
    Given relay "hub" publishes nothing about itself
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    And I watch for notes tagged "p" as "carol"
    Then relay "hub" is holding 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  # ---- when the limit actually binds ------------------------------------

  Scenario: A relay that will hold only two subscriptions gets only two
    # A deliberately extreme limit, because the point is the MECHANISM: three
    # separately-bounded watches cannot be folded into fewer than three
    # subscriptions, so something has to give on a relay that will hold two.
    # What gives is the third -- and the relay is never sent a subscription it
    # said it would not hold.
    Given relay "hub" allows only 2 subscriptions at a time
    When I watch for the latest 10 notes tagged "p" as "alice"
    And I watch for the latest 10 notes tagged "p" as "bob"
    And I watch for the latest 10 notes tagged "p" as "carol"
    Then relay "hub" is known to allow only 2 subscriptions at a time
    And relay "hub" is holding 2 subscriptions
    And relay "hub" refused 1 subscription it could not hold

  Scenario: What could not be requested is said out loud, not quietly dropped
    # THE contract of this feature, and the reason a limit is safe to enforce
    # at all. The watch that did not fit is not left looking live and empty:
    # the app is told, in that watch's own evidence, that what it asked for
    # could not be requested here. Silent truncation is the worst failure
    # class in this system -- everything downstream believes the request is
    # live -- and a limit that produced it would be worse than no limit.
    Given relay "hub" allows only 2 subscriptions at a time
    When I watch for the latest 10 notes tagged "p" as "alice"
    And I watch for the latest 10 notes tagged "p" as "bob"
    And I watch for the latest 10 notes tagged "p" as "carol"
    Then 1 of my watches is told it could not be requested in full
    And relay "hub" refused 1 subscription it could not hold

  Scenario: Demand arriving at a full relay is refused, not swapped in
    # A limit must not thrash. Once a relay is full, whatever it is already
    # serving keeps being served: newly arrived demand is what gets refused,
    # not whichever subscription a re-sort happened to demote. Otherwise the
    # limit itself becomes a source of churn -- closing and reopening
    # subscriptions forever while the demand set stays exactly the same.
    #
    # What this scenario can state honestly is the COUNTS, over demand that
    # genuinely arrives at different times. That no established subscription
    # is ever renamed or reopened is a claim about identity across recompiles,
    # which depends on how a document's arrival interleaves with the fourth
    # watch; it is pinned deterministically instead by `nmp-router`'s
    # `a_bound_budget_does_not_churn_what_it_already_serves`, which measures
    # ZERO wire ops when more demand meets a saturated relay.
    #
    # The refusal count is asserted FIRST on purpose. It is the assertion that
    # polls, and the compile that refuses is downstream of the relay's own
    # HTTP fetch -- which the client-to-relay wire cannot see going quiet. A
    # one-shot read of the socket taken before it would be green by luck.
    Given relay "hub" allows only 2 subscriptions at a time
    When I watch for the latest 10 notes tagged "p" as "alice"
    And I watch for the latest 10 notes tagged "p" as "bob"
    And 250ms later I watch for the latest 10 notes tagged "p" as "carol"
    And 250ms later I watch for the latest 10 notes tagged "p" as "dave"
    Then relay "hub" refused 2 subscriptions it could not hold
    And relay "hub" is holding 2 subscriptions

  # ---- the name a subscription is given ---------------------------------

  Scenario: A relay that will not accept our subscription names is reported
    # NMP names every subscription with the same fixed 64-character string --
    # exactly the longest NIP-01 allows. A relay advertising anything shorter
    # would reject every single request we send it, and until now nothing
    # would have noticed: the requests go out, nothing comes back, and the
    # relay looks merely quiet. This is a diagnosis and nothing more. The name
    # is never shortened to fit, because a relay can re-publish its document
    # at any time and a subscription whose name changed underneath it is a
    # subscription nobody can close.
    Given relay "hub" accepts subscription names of at most 32 characters
    When I watch for notes tagged "p" as "alice"
    Then relay "hub" is reported as refusing the names NMP gives subscriptions
    And relay "hub" serves every "p" watch with 1 subscription
    And nothing I asked for was refused for want of a subscription

  Scenario: A relay with room for our subscription names is not reported
    # The control, and the guard against reporting every relay. nostr.wine
    # advertises 71 characters, which is roomier than NIP-01's own cap; there
    # is nothing wrong with it and nothing to say about it.
    Given relay "hub" accepts subscription names of at most 71 characters
    When I watch for notes tagged "p" as "alice"
    Then relay "hub" is not reported as refusing the names NMP gives subscriptions
    And relay "hub" serves every "p" watch with 1 subscription
