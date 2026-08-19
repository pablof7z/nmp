Feature: A source that has finished answering says so
  `features/coverage/absence-settlement.feature` records the owner's ruling on
  what makes an absence knowable:

  > and "do we have a 10002 for these three users" is very knowable: the moment we receive EOSE from the indexer relays we use we know, one way or another, whether we have a 10002 or not.

  > Settlement is that confirmation and nothing else -- not a timeout, not a retry budget, not a heuristic.

  The engine has always computed that moment. It could not report it. An app
  reading a query saw `SourceStatus`, whose members were `Requesting`,
  `Connecting`, `Disconnected`, `AwaitingAuth`, `AuthDenied` and `Error` --
  none of which means "this source finished its request" -- and
  `reconciled_through`, which answers a different question and disagrees with
  this one in both directions. So an app that needed "give me this snapshot
  once its sources have finished" had one thing left to key on, and it was a
  wall clock. That is the heuristic the file above forbids, and a real
  consumer wrote it twice.

  This file adds the missing member and nothing else. Read it beside
  `features/coverage/empty-vs-unknown.feature`, which owns the older half of
  the same distinction: an empty row set is evidence of nothing on its own,
  and no value here may ever collapse these per-source facts into one verdict
  about the query.

  Scenario: A relay that finished with nothing is not a relay that has not answered
    # The two states that used to be one. Both relays are connected, both
    # have sent no rows, and before this one of them was lying by omission.
    Given a query plans relay "finished" and relay "unfinished"
    And relay "finished" confirms end of stored events having sent nothing
    And relay "unfinished" never confirms end of stored events
    When I observe the query snapshot
    Then its local rows are empty
    And relay "finished" reports that it finished its stored events
    And relay "unfinished" reports an outstanding request
    And neither reports any global complete or authoritative-empty state

  Scenario: Finishing is not a claim that anything was proven
    # The boundary that stops the new fact from becoming the old collapsed
    # verdict. A request the caller bounded may claim no interval at all --
    # it still ends, and the relay still sent everything it was going to.
    # "It is done" and "it proved your window" are two facts, and they must
    # be free to disagree.
    Given a query bounds its request with a result limit
    And the relay confirms end of stored events
    When I observe the query snapshot
    Then the relay reports that it finished its stored events
    And the relay reports no proven watermark

  Scenario: Nothing else counts as finishing
    # The negative half, and the reason this is a settlement rather than a
    # progress indicator. Neither a relay that never accepted the question
    # nor one that refused it has finished answering it, and waiting longer
    # does not change that.
    Given a query plans a relay that cannot be reached
    And a relay that refuses the request instead of confirming end of stored events
    When I observe the query snapshot
    Then neither reports that it finished its stored events
    And neither reports a proven watermark
