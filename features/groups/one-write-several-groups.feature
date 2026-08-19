Feature: One write, several groups, through the one publish door
  A kind:30315 session status is addressable at `(author, d=status)` and
  carries one `h` per room the session occupies, so the same status renders in
  every room the agent is in. Publishing it once per room makes each copy
  REPLACE the last -- they share the coordinate -- so a multi-`h` write is not
  a convenience, it is the only correct shape for that event.

  `Group` is one relay scope plus one group id, and its doors enforced that, so
  this write had no door at all. The consumer that needed it hand-built a
  `WriteIntent`, spelling its own `WriteRouting::Explicit` and writing its own
  `h` rows -- exactly what #1242 removed for every other group write, and the
  last piece of protocol logic left in that app.

  `Groups` closes it: the hosts a scope named plus the SET of ids one event
  claims, with ONE method, `publish`. NMP appends the rows, NMP signs, NMP
  publishes, and the app reads its own write back through the subscription it
  already holds.

  There is deliberately no pre-signed door and no mint-without-publish door.
  The pre-signed one is unnecessary: the consumer's unsigned path already
  computed the event id without signing (an id is a hash of
  `(author, created_at, kind, tags, content)` and a signature was never an
  input), and the refusal that pushed status onto its signed path said, in so
  many words, that exact multi-group events must be pre-signed -- an ARITY
  limit, not an id-timing requirement. The mint-without-publish one buys
  nothing: a `WriteIntent` derives nothing at all, so it cannot be persisted
  across a restart, cloned for batching, or inspected. An app that wants NMP
  to sign without publishing uses the engine's own sign-only door.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  @nip29
  Scenario: One event carries one h row per named group
    When I publish an event of kind 30315 into the groups "darkroom" and "photographers"
    Then the delivered event carries one h row per named group
    And the app named no relay and no h row

  @nip29
  Scenario: Publishing once per room would collide on one replaceable coordinate
    Then the same status published once per room would share one addressable coordinate

  @nip29
  Scenario: The composed bytes do not depend on how the caller spelled the set
    Then naming the same rooms in another order composes the identical rows

  @nip29
  Scenario: A write context over no group is never formed at all
    Then naming no group at all forms no write context

  @nip29
  Scenario: The h row belongs to the retained scope at this arity too
    Given an unsigned event of kind 30315 with content "working"
    And that event already carries an h tag with value "photographers"
    When I publish that event into the groups "darkroom" and "photographers"
    Then the publication is refused with a typed caller-supplied-h error
    And no relay received the event

  @nip29
  Scenario: A several-group write is an ordinary tracked write with no lifecycle of its own
    When I publish an event of kind 30315 into the groups "darkroom" and "photographers"
    Then the write carries a store-issued receipt id
    And it was delivered to every host the scope named and to no other relay
