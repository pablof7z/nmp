Feature: When there is nowhere to publish, say so
  Every other scenario in this arm ends with somewhere to send an event. This
  one is what happens when the three sources are all exhausted and the answer
  is genuinely nothing: the author's relay list settled as absent, no app
  relays configured, and no p-tagged recipient with a reachable inbox.

  This is NOT the cold-start case. Nothing here is waiting on a fetch --
  discovery ran, the sources finished, and the resolver's answer is final and
  empty. There is no knowledge left to acquire that would change it, which is
  precisely why it cannot be treated as a park and left quiet.

  Two wrong answers are ruled out, and they fail in opposite directions.

  Staying SILENT is wrong because the app did everything correctly and has no
  way to find out that its user's message went nowhere. Pablo's ruling on this
  class of failure -- made about a DM to a recipient with no discoverable
  relay list, and general to all of it:

  > This way of popping up a "we were trying to publish to relay X and it didn't work" or "we were trying to route this event but it didn't work" needs ato exist because there are many ways we'll find ourselves there

  GUESSING is wrong because there is no relay the engine could pick that the
  user or operator chose. Substituting a well-known public relay would publish
  someone's event to a host nobody in the chain consented to -- and it would
  make the misconfiguration invisible, because the write would appear to
  succeed. The engine ships no relay list of its own and must not acquire one
  here.

  Every scenario here is @designed.

  Background:
    Given I am logged in as my own account
    And the indexers have finished their stored events without a relay list for my own account
    And no app relays are configured
    And no fallback relays are configured

  # ---- the refusal ------------------------------------------------------

  @designed
  Scenario: Nothing to work with is a stated refusal, not a silent drop
    # The floor. Three empty sources and a settled answer, so the publish is
    # told, in the same terms it would be told about a rejecting relay, that
    # there is no destination.
    When I publish a note saying "into the void"
    Then the note is routed to no relay
    And the publish reports that no destination could be determined
    And the note is never reported as sent

  @designed
  Scenario: The reason names what was missing, not merely that something was
    # "Stuck" and "stuck because X" are different messages, and only the
    # second one an app or an operator can act on. All three exhausted sources
    # are named, because any one of them being configured would have produced
    # a route -- so the reason doubles as the list of ways to fix it.
    When I publish a note saying "into the void"
    Then the publish reports that no destination could be determined
    And the reason names that my own relay list is absent
    And the reason names that no app relays are configured

  @designed
  Scenario: Recipients with no inbox between them do not rescue the route
    # The same refusal with the p-tag source exercised rather than empty.
    # Bob and Carol are settled as having no relay list, which is a resolved
    # answer contributing nothing -- so the route is complete, empty, and
    # refused rather than left open on their account.
    Given the indexers have finished their stored events without a relay list for Bob
    And the indexers have finished their stored events without a relay list for Carol
    When I publish a note saying "none of us has a relay" that p-tags Bob and Carol
    Then the note is routed to no relay
    And the publish reports that no destination could be determined

  # ---- what must never be substituted -----------------------------------

  @designed
  Scenario: No public relay is ever substituted for an empty answer
    # The engine carries no built-in, hardcoded, or bootstrap relay set, and
    # this is the scenario that would catch one being introduced as a
    # kindness. A relay nobody configured is a relay nobody consented to
    # publish through, and an event that quietly reaches one is worse than an
    # event that reaches nothing and says so.
    When I publish a note saying "into the void"
    Then no relay is ever contacted for the note
    And no relay outside the ones configured is ever contacted

  @designed
  Scenario: The indexers are not a publishing destination of last resort
    # The specific guess most available to an implementation: the discovery
    # indexers are configured, connected, and right there. They are where the
    # engine ASKS about relay lists, never where it publishes an ordinary
    # event -- and "indexers are never a content fallback" cuts this way too.
    Given 2 indexer relays are configured
    When I publish a note saying "into the void"
    Then the note is routed to no relay
    And the note is never routed to either indexer

  # ---- the controls -----------------------------------------------------

  @designed
  Scenario: One configured app relay is the difference between refusal and a route
    # The refusal is about having nothing, not about a policy that forbids
    # thin routes. Configure a single app relay and the same publish, with the
    # same absent relay list, resolves and completes.
    Given app relays "app-indexer" are configured
    When I publish a note saying "somewhere after all"
    Then the note is routed to exactly "app-indexer"
    And routing is complete
    And the publish reports no routing problem

  @designed
  Scenario: One reachable recipient is enough, even with nothing else
    # The other single-source control. The author has no relay list and the
    # operator configured nothing, but Bob has an inbox -- so the note has
    # somewhere to be, and the resolver is not entitled to refuse merely
    # because the author's own source came back empty.
    Given Bob's relay list names "bob-inbox" as his read relay
    When I publish a note saying "at least you will see this" that p-tags Bob
    Then the note is routed to exactly "bob-inbox"
    And routing is complete

  # ---- staying visible --------------------------------------------------

  @designed
  Scenario: The refusal is still legible after a restart
    # A message the app was not holding a receipt for when it arrived is a
    # message nobody received. The reason has to survive the process that
    # produced it, because the common way to find out about this is to open
    # the app later and wonder why a note never appeared anywhere.
    Given I published a note saying "into the void"
    When I reconstruct the engine from the same durable store
    Then the publish still reports that no destination could be determined
    And the reason still names that my own relay list is absent

  @designed
  Scenario: A publish with no destination shows up as a stalled write
    # Per-receipt reporting answers "what happened to THIS note" and needs
    # someone to be asking. The global question -- "is anything quietly stuck"
    # -- is what surfaces a misconfigured app, where EVERY write lands here
    # and no single receipt tells the operator that.
    Given I published a note saying "into the void"
    Then diagnostics reports the note among the stalled writes
    And its stalled entry carries the same reason and how long it has been so
