Feature: An app-owned store commits a whole session before it becomes live
  Session persistence is optional. When an app supplies it, the app's store is
  the durable authority for one opaque whole-session value and revision. NMP
  prepares a complete candidate, asks the store to compare-and-replace the
  previous revision, and changes the live session only after that exact write
  is confirmed.

  Storage work never runs under the reducer, network, or shared-runtime owner.
  A failure before commit leaves the prior session live. An outcome that might
  have committed is different: NMP refuses further persisted mutations until
  the engine is reopened from the authoritative store instead of guessing or
  writing stale memory back over it.

  Rule: Persistence confirmation is the activation boundary

    # nmp:id=IDENTITY-SESSION-STORE-001
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: A persisted session mutation activates only after save succeeds
      Given the app supplied a session store holding revision A
      When adding and selecting an account produces candidate revision B
      Then revision A remains the complete live session while B is being saved
      And B becomes live only after the store confirms its commit

    # nmp:id=IDENTITY-SESSION-STORE-002
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: A failed save leaves the previous live session unchanged
      Given the app supplied a session store holding revision A
      When a session mutation is rejected, conflicts, or fails before commit
      Then the mutation returns that exact storage outcome
      And revision A remains the complete live session
      And no candidate provider can serve a write

    # nmp:id=IDENTITY-SESSION-STORE-003
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Durable storage wins after a crash between save and activation
      Given the app store committed candidate revision B
      When the process dies before B becomes live
      And a new engine opens from the same store
      Then revision B is restored as the complete session
      And revision A is never written back over it

  Rule: Opening and mutation execution preserve owner boundaries

    # nmp:id=IDENTITY-SESSION-STORE-004
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Engine open restores the complete session before starting work
      Given durable writes and network demand exist in NMP storage
      And the app session store contains a valid whole session
      When a new engine opens
      Then the complete session is restored before networking or write recovery
      And no partial account or provider set is observable during open

    # nmp:id=IDENTITY-SESSION-STORE-005
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Omitting a session store means intentional in-memory operation
      Given the engine was opened without session persistence
      When an account is added and selected
      Then the mutation activates without a storage request
      And a new engine does not restore it
      And no persistence error or durability claim is produced

    # nmp:id=IDENTITY-SESSION-STORE-006
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Ambiguous commit refuses further persisted mutation until reopen
      Given a persisted session mutation may have committed in app storage
      When NMP cannot know the commit outcome
      Then the candidate does not become live in the current engine
      And later persisted session mutations are refused as recovery required
      And reopening loads the app store without writing stale live state first

    # nmp:id=IDENTITY-SESSION-STORE-007
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Storage code can reenter without owning an engine lock
      Given a session storage request is in progress
      When the app store inspects the committed session or shuts the engine down
      Then that engine call does not wait for the storage request
      And the storage completion has one typed terminal outcome

    # nmp:id=IDENTITY-SESSION-STORE-008
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: A reentrant session mutation is refused before storage
      Given one session mutation owns an active storage request
      When that storage request attempts another mutation on the same session
      Then the nested mutation is refused as reentrant before another storage request
      And the outer mutation remains the sole transaction owner

    # nmp:id=IDENTITY-SESSION-STORE-009
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Concurrent mutations preserve submission order
      Given the app supplied a session store holding revision A
      When two session mutations are submitted concurrently
      Then their whole-session save requests arrive in submission order
      And the second request expects the revision committed by the first request
      And each candidate becomes live only after its own save commits

    # nmp:id=IDENTITY-SESSION-STORE-010
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1398
    Scenario: Cancellation distinguishes known and ambiguous outcomes
      Given the app supplied a session store holding revision A
      When a mutation is cancelled before its storage request is delivered
      Then it ends as cancelled without storage I/O or a live-session change
      When another mutation is cancelled after its save begins
      Then its candidate never becomes live
      And later mutations require reopening because the durable commit outcome is unknown
