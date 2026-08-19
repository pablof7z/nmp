Feature: A message the user just sent is in the feed before any relay has answered
  Pressing send is one moment for the person doing it and two facts for NMP:
  the event was accepted into the outbound publication queue, and some relay
  did or did not carry it. Those are different facts, they resolve at
  different times, and conflating them is what makes a feed lie.

  > the user publishes something and it immediately shows up in any LiveQuery
  > matching that filter. If the app checks the source for where that event
  > came through, it shows as coming from the cache and 0 relays. As relays
  > accept or reject it, where the event loaded from is reflected. If a chat
  > input or any publish button wants to say "did the event publish fire off"
  > -- we show a completion -- yes, the moment we accept the event it should
  > show as "it will publish".

  So the answer to "is it on screen" is decided at acceptance, and the answer
  to "who carried it" is reported honestly and separately: the cache, and
  zero relays, until a relay actually does. The two answers never wait for
  each other. An app can render the message and a "sending..." chip off the
  same publish, and neither is guessing.

  None of this is a group feature. It is how every publish in NMP works --
  a note, an article, a chat message, anything -- and no protocol module
  contains a line of code implementing it. A rule that lived in one protocol
  would be a rule every other protocol silently lacked.

  What it is NOT is a licence to show accepted writes indiscriminately. The
  event appears in the queries whose filters it matches, and in no others,
  exactly like every row that ever came off a relay. And a row ANOTHER host
  served still never answers for a host that did not serve it -- that
  isolation is about FOREIGN data, and our own write never becomes foreign.
  Whether a pinned feed shows a row turns on who wrote it, not on who has
  carried it so far; which relays have carried it is reported, separately
  and honestly, as the row's provenance.

  Background:
    Given I am logged in as my own account
    And my write route names hosts "host-a" and "host-b"

  # ---- the moment of sending -------------------------------------------

  Scenario: The message is on screen before any host could possibly have answered
    Given neither "host-a" nor "host-b" can be reached
    And I am watching a feed whose filter the message matches
    When I send a message
    Then the feed shows the message
    And the row reports the cache as its source
    And the row names zero relays

  Scenario: "It will publish" is answered at acceptance, not at the first ack
    # What a chat input's send button actually asks. The completion an app
    # shows is the acceptance of the obligation, which NMP knows on its own;
    # it is not a promise about the world, which NMP cannot make yet.
    Given neither "host-a" nor "host-b" can be reached
    When I send a message
    Then the publish reports the message accepted
    And it reports that before any host has answered
    And no host is yet reported as having carried it

  # ---- as the world answers ---------------------------------------------

  Scenario: Provenance names exactly the hosts that carried it, as they carry it
    Given I am watching a feed whose filter the message matches
    And I have sent a message that is on screen naming zero relays
    When "host-a" accepts the message
    And "host-b" refuses the message
    Then the row names "host-a" as its only source
    And "host-b" is never named as a source
    And the feed still holds exactly one copy of the message

  Scenario: A message every host refused is still the user's message
    # The only outcome in which the source set stays empty permanently rather
    # than momentarily, which is what makes it the case that asks whether
    # "zero relays" means "not yet" or "never". For visibility it means
    # neither. The refusals are on the receipt, in each host's own words,
    # which is where an app reads them and where it can offer a retry. A feed
    # that deleted the text to convey the same thing would be conveying it
    # worse and destroying what the user wrote.
    Given I am watching a feed whose filter the message matches
    And I have sent a message that is on screen naming zero relays
    When "host-a" refuses the message
    And "host-b" refuses the message
    Then the receipt reports "host-a" refused it, in that host's own words
    And the receipt reports "host-b" refused it, in that host's own words
    And the feed still shows the message
    And the row still names zero relays

  # ---- which feeds see it ------------------------------------------------

  Scenario: Sending and then opening the feed shows the message, and so does a second feed
    # Two doors, one answer. An app that sends from a composer and then
    # navigates to the conversation is the ordinary case, not an edge case,
    # and it is served by the initial snapshot rather than by a delta. A
    # mechanism that only notified whichever subscription happened to be live
    # would satisfy every other scenario here and fail every app that opens a
    # screen after sending.
    Given I am watching a feed whose filter the message matches
    And a second feed on the same selection is also open
    When I send a message
    Then both feeds show the message naming zero relays
    When I open a third feed on the same selection
    Then it also shows the message naming zero relays

  Scenario: A feed the message does not match never shows it
    # The guard on the whole mechanism. "Immediately" qualifies WHEN, never
    # WHERE. A locally accepted write is filtered exactly like a row that
    # arrived from a relay, and an implementation that special-cased it into
    # visibility would put the user's note into somebody else's screen.
    Given I am watching a feed whose filter the message matches
    And I am watching another feed whose filter it does not match
    When I send a message
    Then the matching feed shows the message
    And the other feed does not show the message

  # ---- across a restart --------------------------------------------------

  Scenario: A message still in flight is still in the feed after a restart
    # The row and the obligation have to agree. NMP is still going to publish
    # this message, so a feed that forgot it on restart would be hiding live
    # work from the person who asked for it -- and losing their text while
    # continuing to send it.
    Given I have sent a message that no host has answered yet
    When the process stops immediately
    And I reconstruct the engine from the same durable store
    And I open a feed whose filter the message matches
    Then the feed shows the message
    And the row still names zero relays

  # ---- what this does not weaken ----------------------------------------

  Scenario: A row another host served still never answers for a host that did not serve it
    # The isolation this must not cost. It was always a rule about FOREIGN
    # data -- one host's cached rows answering a question about a different
    # host -- and a row nobody has served is not foreign data. Showing our own
    # unsent write and refusing another host's row are answers to two
    # different questions, and only the second one is about isolation.
    Given a row that only "host-b" has ever served
    When I read from a selection pinned to "host-a" alone
    Then that row is not among the results
    And a row no host has served is still among them

  Scenario: A page of a pinned feed counts the unsent message like any other row
    # The consequence an app notices without knowing why: if a locally
    # accepted row were admitted but not counted, a page of ten would deliver
    # eleven; if it were filtered out of an already-bounded page, a page of
    # ten would deliver nine and the feed would look like it had lost
    # messages. Visibility is decided before the bound, never after.
    Given a feed pinned to "host-a" that pages 2 rows at a time
    And the newest matching row is a message I just sent that no host has carried
    When I read the newest page
    Then it holds exactly 2 rows
    And my unsent message is the first of them

  # ---- ours versus foreign ----------------------------------------------

  Scenario: The user's own message is not withdrawn because of what an unrelated host did
    # Visibility under a pin asks whether a row is OURS, never whether some
    # relay has carried it yet. A locally accepted write keeps its local
    # origin forever, so the answer cannot change under it: with "host-b"
    # refusing either way, "host-a" staying silent and "host-a" accepting
    # give the same feed. Only the reported provenance differs, which is the
    # one thing "host-a" is entitled to change.
    #
    # The alternative reading -- shown while uncarried, withdrawn once
    # carried -- gave two answers to that one question, and the deciding
    # fact was a host the feed was not even watching. Reachable through the
    # general pinned/explicit primitives whenever an app watches a strict
    # subset of the hosts it publishes to; it was never reachable through
    # the group door, whose read pin and write scope are one host set by
    # construction, which is why it survived #1173 and #1182 both.
    Given I am watching a feed pinned to "host-b" alone
    And I send a message routed to both "host-a" and "host-b"
    And the feed shows the message naming zero relays
    When "host-b" refuses the message
    And "host-a" accepts the message
    Then the feed still shows the message
    And the row names "host-a" as its source
    And a row I did not write, carried only by "host-a", is still not shown

  # ---- and none of it belongs to a protocol -----------------------------

  Scenario: Every publish gets this, and no protocol implements it
    # The rule the ruling is most insistent about. If this behaviour lived in
    # a protocol module it would be a behaviour every other protocol quietly
    # lacked, and an app would learn which by discovering that its notes
    # behave differently from its chat messages.
    Given I am watching a feed whose filter the message matches
    When I send an ordinary note
    And I send an ordinary long-form article
    Then each appears immediately naming zero relays
    And no protocol module decides whether a published event may be seen
