Feature: Asking what a write would compute to, before committing to anything
  An app has to answer one question before it lets someone press send: where
  would this actually go? "Where did it go" is a different question and it
  arrives too late to grey out a button.

  > all we  can do is provide via the nip17 crate a "what will this compute
  > to" so that apps can easily show which relays would be used for a certain
  > communication, for example, such that they can disable the send button
  > when a relay cannot be determined for one of the parties

  That quote is where this feature came from and is kept verbatim, but the
  crate it names is not being built (#1023). What it asks for was never a DM
  property: every scenario below reaches it through ordinary NIP-65 relay
  lists and explicitly addressed writes, which is what NMP actually routes
  with. Nothing here waits on a protocol module.

  So a preview is a pure question. Asking it accepts nothing, signs nothing,
  records nothing durable and produces no receipt -- an app may ask it on
  every keystroke of a compose screen. The one deliberate exception is that
  asking causes the missing answer to be sought, so that asking again a
  moment later can succeed. Opening the compose screen is what fetches the
  recipient's relay list; by the time the user stops typing the question has
  already answered itself.

  Everything else here rests on one property: preview and real routing cannot
  disagree. A preview naming different destinations than the publish that
  follows it would be worse than no preview at all, because the button would
  be enabled on evidence the send path does not use. There is one derivation
  and one resolver and both callers go through it -- so every scenario below
  that publishes after previewing is a falsifier for a second, drifting code
  path having grown.

  A preview reports three things and they are not the same thing: the
  destinations known so far, whether anything is still unknown, and whether a
  recipient is outright blocked -- settled as having no reachable destination
  at all. Unknown is "we have not been told yet"; blocked is "we asked, and
  the answer is that there is nowhere to send". Only the second is a
  permanent no, and an app that cannot tell them apart cannot write a
  sensible footer.

  Background:
    Given only the indexer relay "wss://indexer.example" is configured
    And my relay list names "wss://relay.mine.example" as my write relay

  # ---- a preview is a question, not an act ------------------------------

  Scenario: Asking what a note would route to writes nothing
    Given I am logged in as my own account
    When I ask what a note saying "hello" would route to
    Then the preview names "wss://relay.mine.example"
    And no write was accepted
    And no receipt was created
    And nothing durable was recorded

  Scenario: A thousand questions leave the write plane as they found it
    # The compose screen previews on every keystroke. If preview cost anything
    # durable, the cheapest possible app behaviour would be the most expensive
    # one, and apps would learn to ask rarely -- which defeats the point.
    Given I am logged in as my own account
    When I ask what a note saying "hello" would route to 1000 times
    Then every answer is the same answer
    And no write was accepted
    And nothing durable was recorded

  # ---- preview and real routing cannot disagree -------------------------

  @ledger-3
  Scenario: An outbox preview names what the publish then uses
    # THE safety property of this feature, and both halves are read from the
    # same run: what the preview said, and where the write was actually sent.
    Given Alice's relay list names "wss://alice.example" as her read relay
    And I am logged in as my own account
    When I ask what a note p-tagging Alice would route to
    And I publish that same note
    Then the note goes to exactly the destinations the preview named

  @ledger-3
  Scenario: An explicitly addressed preview names what the publish then uses
    # Nothing to resolve at all here, which is exactly why it is worth
    # pinning: the trivial case must go through the same one derivation. It
    # is also the second strategy the safety property needs -- Auto and
    # Explicit are the two there are, and a preview has to agree with both.
    Given I am logged in as my own account
    When I ask what a note addressed only to "wss://archive.example" would route to
    And I publish that same note
    Then the note goes to exactly the destinations the preview named

  @ledger-3
  Scenario: A destination the preview could not name is not conjured at publish
    # The other direction of the same property, and the one that actually
    # burns an app: previewing blocked, sending anyway, and the write quietly
    # succeeding somewhere the app was told was unreachable. Reaching it
    # needs every contributing source exhausted, my own outbox included --
    # otherwise my write relay answers the question and there is nothing to
    # be blocked about.
    Given the indexers have settled that I have no relay list
    And the indexers have settled that Bob has no relay list
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then the preview reports Bob as having no reachable destination
    When I publish that same note
    Then the note goes to no destination at all

  # ---- what a preview reports when it does not know ---------------------

  Scenario: What is known so far is reported even while something is unknown
    # Half an answer is worth rendering. The app can show "will go to my relay
    # and Alice's" while the third recipient is still resolving, and that is a
    # different screen from showing nothing at all.
    Given Alice's relay list names "wss://alice.example" as her read relay
    And nothing is known yet about Carol's relay list
    And I am logged in as my own account
    When I ask what a note p-tagging Alice and Carol would route to
    Then the preview names "wss://relay.mine.example"
    And the preview names "wss://alice.example"
    And the preview reports a destination still unknown for Carol

  Scenario: An unknown destination is not the same as no destination
    # Unknown is temporary and blocked is permanent, and an app renders them
    # differently: "still working it out" versus "this person cannot be
    # reached". Collapsing them into one flag makes the second unsayable.
    Given nothing is known yet about Bob's relay list
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then the preview reports a destination still unknown for Bob
    And the preview does not report Bob as having no reachable destination

  Scenario: A recipient with no relay list is unreachable, not unknown
    # The settled negative. The indexers were asked and they finished
    # answering: Bob has never published a relay list. That is knowledge,
    # not absence of it, and it is the case the send button exists for.
    Given the indexers have settled that I have no relay list
    And the indexers have settled that Bob has no relay list
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then the preview reports Bob as having no reachable destination
    And the reason names Bob's missing relay list
    And the preview reports nothing still unknown

  Scenario: A relay that has not answered yet does not make the preview lie
    # Why one-shot is enough. A preview does not report "unknown" because a
    # response is merely in flight -- it settles when the discovery sources
    # finish answering, which is the moment the answer is known either way.
    # The quote is Pablo's, about the relay list it was originally asked of;
    # the settling rule it states is the same for any relay list.
    #
    # > since the preview would complete once we EOSE from the relays anyway,
    # > so the "400ms later bob's 10050 arrives would imply that bob actually
    # > published their 10050 400ms after we checked for it, not that the
    # > relay responded *after* we checked
    #
    # So the send button is never stuck disabled by a slow relay. It can only
    # be disabled because the relay list genuinely was not published yet.
    Given relay "wss://indexer.example" never finishes answering
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then the preview does not answer while the indexer is still answering
    And the preview never reports Bob as having no reachable destination

  # ---- the call site this exists for ------------------------------------

  Scenario Outline: The app can decide whether to allow a send at all
    # Pablo's example, spelled out as the three-way decision an app actually
    # makes. NMP does not own the policy -- it owns the evidence -- but the
    # evidence has to be enough to make the choice without guessing. My own
    # outbox is settled empty so the verdict turns entirely on what is known
    # about the recipient, which is the case the send button exists for.
    Given the indexers have settled that I have no relay list
    And <knowledge>
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then the app can tell that sending is <verdict>

    Examples:
      | knowledge                                            | verdict |
      | Bob's relay list names "wss://inbox.bob.example"     | allowed |
      | nothing is known yet about Bob's relay list          | not yet |
      | the indexers have settled that Bob has no relay list | refused |

  # ---- asking the question starts producing the answer ------------------

  Scenario: A preview that could not answer has set the answer in motion
    # The deliberate impurity, and the whole reason one-shot is livable.
    # Opening a compose screen previews in order to render the button, and
    # that preview is what causes the recipient's relay list to be fetched.
    Given nothing is known yet about Bob's relay list
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then the preview reports a destination still unknown for Bob
    And Bob's relay list is now being sought
    When Bob's relay list arrives naming "wss://inbox.bob.example"
    And I ask again what a note p-tagging Bob would route to
    Then the preview names "wss://inbox.bob.example"
    And the preview reports nothing still unknown
    And no write was accepted by either question

  Scenario: Previewing only ever widens what is being sought
    # Widen-only, because preview is called constantly and from screens that
    # come and go. A preview that could narrow the discovery set would let
    # closing a compose screen tear down knowledge the feed behind it needs.
    Given my feed is already seeking Alice's relay list
    And I am logged in as my own account
    When I ask what a note p-tagging Bob would route to
    Then Bob's relay list is now being sought
    And Alice's relay list is still being sought

  # ---- a preview needs nobody in particular -----------------------------

  Scenario: A preview works before anyone has signed in
    # "What will this compute to" must be answerable on a screen that exists
    # before an identity does -- and it is, because routing derives from the
    # kind and the tags, with the author an optional part of the question
    # rather than a precondition for asking it.
    Given no account is current
    When I ask what a note p-tagging Alice would route to
    Then the preview answers
    And no identity was resolved
    And no write was accepted

  Scenario: Previewing never asks a signer for anything
    # A preview of an unsigned draft must not be the thing that pops a
    # hardware signer prompt, or apps will stop previewing.
    Given a NIP-46 signer is registered for the current pubkey
    And I am logged in as my own account
    When I ask what a note saying "hello" would route to
    Then the preview answers
    And the signer was never asked to sign anything
    And nothing was signed
