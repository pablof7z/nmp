Feature: App relays are additive for every kind, every author, always
  The operator configures a set of relays the app itself wants everything to
  reach -- its own indexer, its own archive, the relay its search runs over.
  `RelayDirectory::app_relays`'s shipped doc comment
  (`crates/nmp-router/src/facts.rs:81-91`) states the policy in six words that
  leave nothing to interpret:

  > Operator-configured app relay set (`Lane::AppRelay`, §2.1 of `routing-and-ownership.md`) -- every kind, every author, always, additive, never counted toward the 2-relay-min.

  The read path honours that today. The write path does not call the accessor
  at all. So this is not a new policy being invented for writes; it is the
  policy already written down, applied on the side that ignores it.

  "Additive" is the word doing the work. App relays are not a fallback for a
  thin relay list, not a substitute when discovery is cold, and not a
  per-kind opt-in. They are added to whatever else the resolver found, or to
  nothing, unconditionally.

  This closes a consumer request that was once refused. @lima-codex asked for
  kind:0 profile copies to reach the configured app/indexer relays and was
  told it was an out-of-scope pinned-route request. Under the designed default
  it is not a feature at all -- a kind:0 under `Auto` reaches the app relays
  because EVERY kind does, with no exception, no special case, and no route
  the app ever names. Recorded here so nobody re-adds a kind:0-shaped special
  path; if a future change makes profiles reach the app relays by some
  dedicated mechanism, the model has been broken and patched rather than used.

  Every scenario here is @designed.

  Background:
    Given I am logged in as my own account
    And my relay list names "author-write-1" and "author-write-2" as my write relays
    And app relays "app-indexer" and "app-archive" are configured

  # ---- the request that falls out for free ------------------------------

  @designed
  Scenario: A profile publish reaches the app's indexer with no route the app ever named
    # The @lima-codex case, as the consumer would state it: "my profile edits
    # should show up in my app's own search". The app publishes a kind:0 the
    # ordinary way, says nothing about relays, and the copy lands on the
    # configured indexer alongside the author's own write relays.
    When I publish my profile
    Then the profile is routed to exactly "author-write-1", "author-write-2", "app-indexer", and "app-archive"
    And routing is complete

  @designed
  Scenario: A profile publish is not a special case in any way a test can see
    # The guard on the scenario above. A kind:0 must route by exactly the same
    # derivation as a kind:1 -- same sources, same union -- so that "profiles
    # go to the indexer" cannot be satisfied by a kind:0 branch somewhere. The
    # two routes below are identical because nothing in the resolver looks at
    # the kind at all.
    When I publish my profile
    And I publish a note saying "an ordinary note"
    Then the profile and the note are routed to the same relays

  # ---- every kind ------------------------------------------------------

  @designed
  Scenario Outline: Every kind reaches the app relays
    # "Every kind, every author, always." The rows span the shapes that
    # usually tempt someone into a special case: metadata, contacts, ordinary
    # text, reactions, long-form addressable content, and a kind no module in
    # the process has ever heard of. The last row is the important one -- a
    # kind with no registered resolver routes by the built-in outbox rules,
    # and the app relays are part of those rules.
    When I publish a kind <kind> event
    Then the event is routed to "app-indexer"
    And the event is routed to "app-archive"
    And the event is routed to "author-write-1"

    Examples:
      | kind  | what it is                   |
      | 0     | profile metadata             |
      | 1     | a short text note            |
      | 3     | a follow list                |
      | 7     | a reaction                   |
      | 30023 | a long-form article          |
      | 9999  | a kind nothing here knows of |

  # ---- additive, in both directions -------------------------------------

  @designed
  Scenario: App relays are added to a healthy relay list, not held in reserve
    # The anti-fallback assertion. An author with plenty of write relays still
    # gets the app relays; there is no coverage threshold above which they
    # stop being applied. (Fallback relays behave the opposite way, on
    # purpose -- see outbox-fallback-coverage.feature.)
    Given my relay list also names "author-write-3" and "author-write-4" as write relays
    When I publish a note saying "four of my own already"
    Then the note is routed to "app-indexer"
    And the note is routed to "app-archive"

  @designed
  Scenario: App relays are added on top of the recipient fan-out too
    # Composition with source 3. The app's own relays do not replace, narrow,
    # or stand in for a recipient's inbox; all of it lands.
    Given Bob's relay list names "bob-inbox" as his read relay
    When I publish a note saying "for Bob, and for the app" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer", "app-archive", and "bob-inbox"

  @designed
  Scenario: An author with nothing of their own still reaches the app relays
    # The other end of "additive": added to nothing is still added. This is
    # also the shape that makes an author with no write relays routable at
    # all, which outbox-recipients-and-settlement.feature pins as a completion
    # property rather than a coverage one.
    Given my relay list declares no write relays
    When I publish a note saying "nothing of my own"
    Then the note is routed to exactly "app-indexer" and "app-archive"
    And routing is complete

  # ---- the control -----------------------------------------------------

  @designed
  Scenario: An app that configured no app relays gets none invented for it
    # The resolver holds no built-in relay set of its own. "Always additive"
    # means the operator's set is always added; it never means there is a
    # default set to add. An empty configuration contributes exactly nothing.
    Given no app relays are configured
    When I publish a note saying "just me, then"
    Then the note is routed to exactly "author-write-1" and "author-write-2"
