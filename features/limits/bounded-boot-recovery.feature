Feature: Boot recovery costs what changed, not what accumulated
  Reopening an engine rebuilds volatile ownership from the durable write
  queue, and the engine thread finishes that rebuild before it reads its
  first command. So whatever recovery costs, the app's first call pays --
  and a rebuild whose cost follows the SIZE of the queue rather than the
  number of facts it has to record gets slower forever.

  Rule: Recovery records only facts that are not already durable

    Scenario: Reopening over a large queue rewrites nothing
      Given a durable write queue whose every lane is already eligible and unreached
      When the engine reopens and rebuilds ownership from that queue
      Then every lane keeps the exact revision and state it had before the reopen
      And the relays those lanes need are demanded as usual

  Rule: An obligation nothing can want is not carried to the next boot

    Scenario: Repeated presence renewals leave one obligation
      Given an app renews its kind 30315 status many times at one address
      And no relay is ever reached
      When the engine reopens and reads the durable write queue
      Then exactly one obligation is open at that address
      And it is the newest renewal
