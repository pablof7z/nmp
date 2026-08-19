Feature: A cancelled row pull loses no transition and repeats none
  An unbounded row observation is a stream of transitions: every frame an app
  applies is an exact step from the frame it applied last. So a pull that is
  cancelled must leave the observation exactly where it was, and a pull the app
  actually received must never come back. Apps get cancelled constantly --
  timeouts, screens closing, the process being killed -- and every one of those
  moments is a chance to silently drift away from what the engine knows.

  Rule: A transition counts as delivered only once the app really has it

    Scenario: A pull cancelled at the instant its row was produced keeps that row
      Given an app is pulling rows from a live observation
      And the engine has produced the next transition
      When the app is cancelled before it can take that transition
      Then the next pull returns exactly the same transition
      And applying it leaves the app at the state the engine expects

    Scenario: A transition the app received is never handed out again
      Given an app has pulled and acknowledged a transition
      When the app pulls again
      Then it receives the next transition, never the one it already applied

    Scenario: An app killed mid-pull loses nothing it never saw
      Given an app has started a pull and the engine has produced a transition
      When the app disappears without cancelling, acknowledging, or cleaning up
      Then the transition is still waiting for the next pull
      And the observation accepts a new pull immediately

    Scenario: Abandoning an idle pull leaves the observation able to deliver later rows
      Given an app is waiting on a pull and nothing has changed yet
      When the app abandons that pull
      And a row is created afterwards
      Then the next pull delivers that row

    Scenario: A row that arrives while a pull is being cancelled belongs to the next pull
      Given an app is waiting on a pull
      When cancellation and the arrival of a row happen at the same moment
      Then the cancelled pull is told it was cancelled rather than handed the row
      And the next pull delivers that row

  Rule: Cancelling repeatedly costs nothing

    Scenario: A hundred cancel-and-retry cycles hold one transition, not a queue
      Given an app repeatedly starts a pull and cancels it while rows keep changing
      When the app finally lets a pull finish
      Then it receives the one transition it never saw
      And everything that changed in between arrives as a single combined follow-up
      And the app ends up at exactly the engine's current set of rows

  Rule: One pull at a time, and never a shared copy

    Scenario: A second pull on the same observation is refused, even mid-handover
      Given an app is already pulling from an observation
      When something starts a second pull on that same observation
      Then the second pull is refused with a named error
      And it never receives a copy of the first pull's transition

    Scenario: One pull yields at most one row
      Given an app has started a pull
      When anything asks that same pull for a row a second time
      Then it is refused with a named error rather than served a second row
      And it makes no difference whether the first row had arrived yet

  Rule: Finishing a pull the wrong way is refused, and changes nothing

    Scenario: Acknowledging a row that has not arrived is refused and destroys nothing
      Given an app has started a pull that has not produced a row yet
      When the app acknowledges it anyway
      Then it is refused with a named error
      And the pull still delivers its row normally afterwards

    Scenario: A finished pull can never reach into a later one
      Given an app cancelled a pull and started a new one
      When the old pull is acknowledged late
      Then it is refused with a named error
      And the new pull's transition is untouched

    Scenario: Acknowledging, cancelling, and closing at once produce exactly one outcome
      Given an app acknowledges a pull, cancels it, and closes the observation simultaneously
      When all three land together
      Then exactly one of them takes effect and the others report what happened
      And no transition is delivered twice, stranded, or brought back after closing

  Rule: Closing the observation is the end of it

    Scenario: Closing an observation ends a waiting pull and replays nothing afterwards
      Given an app is waiting on a pull
      When the app closes the observation
      Then the waiting pull ends with end-of-stream rather than hanging
      And closing it a second time changes nothing
      And no later pull can bring back a transition the closed observation held

    Scenario: Stopping the engine ends every waiting pull
      Given many observations each have a pull waiting for a row
      When the engine is stopped
      Then every waiting pull promptly reports end-of-stream

    Scenario: Stopping the engine does not eat a transition it already produced
      Given a pull was cancelled and its transition is being held
      When the engine is stopped without the app closing the observation
      Then the next pull still delivers that held transition
      And only then does the app see end-of-stream

  Rule: A windowed view is a picture, not a step

    Scenario: An abandoned windowed view is not replayed, because it was never a step
      Given an app is reading a windowed query, which delivers whole current views
      When the app abandons a pull that had produced a view
      Then that view is not held for the next pull
      But the app has lost nothing it needed, because the next view it receives is complete on its own

  Rule: The handshake stays out of the app's way

    Scenario: Cancelling after a row is acknowledged ends the observation rather than skipping the row
      Given the app's toolkit has acknowledged a transition on the app's behalf
      When the app is cancelled before that transition reaches it
      Then the whole observation is withdrawn
      And no later pull continues from a transition the app never applied

    Scenario: The row is acknowledged before anything else can interrupt
      Given a platform SDK has just received a transition
      When it prepares that transition for the app
      Then it acknowledges the transition first, before any step that could be cancelled

    Scenario: An app across the platform boundary sees exactly what an in-process reader sees
      Given the same query is read in-process and through a platform SDK against one relay
      When both read to completion
      Then they end up with identical rows and identical evidence for them
