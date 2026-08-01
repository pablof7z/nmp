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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_publish_to_two_unreachable_hosts_appears_at_once_reporting_zero_relays
  # nmp:falsifier=withholding a locally accepted row from a pinned projection until some relay has carried it -- dropping the ours clause from nmp_store::Provenance::visible_under_pin -- makes a_publish_to_two_unreachable_hosts_appears_at_once_reporting_zero_relays time out waiting for a row that can never arrive, because neither host is reachable
  Scenario: The message is on screen before any host could possibly have answered
    Given neither "host-a" nor "host-b" can be reached
    And I am watching a feed whose filter the message matches
    When I send a message
    Then the feed shows the message
    And the row reports the cache as its source
    And the row names zero relays

  # nmp:id=WRITES-OPTIMISTICPUBLISH-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_publish_to_two_unreachable_hosts_appears_at_once_reporting_zero_relays
  # nmp:falsifier=deferring the acceptance fact until a first relay ACK makes a_publish_to_two_unreachable_hosts_appears_at_once_reporting_zero_relays hang on its receipt drain, which runs against two deliberately unreachable hosts precisely so no ACK can ever rescue it
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_accepting_host_enters_provenance_a_rejecting_one_never_does_and_the_row_is_never_duplicated
  # nmp:falsifier=letting a refusing host enter the row's source set, or announcing the carried event as a second row rather than growing the first one's provenance, makes an_accepting_host_enters_provenance_a_rejecting_one_never_does_and_the_row_is_never_duplicated see a source set that is not exactly the accepting host, or an added-count of 2
  Scenario: Provenance names exactly the hosts that carried it, as they carry it
    Given I am watching a feed whose filter the message matches
    And I have sent a message that is on screen naming zero relays
    When "host-a" accepts the message
    And "host-b" refuses the message
    Then the row names "host-a" as its only source
    And "host-b" is never named as a source
    And the feed still holds exactly one copy of the message

  # nmp:id=WRITES-OPTIMISTICPUBLISH-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_event_every_host_refused_stays_visible_reporting_zero_relays
  # nmp:falsifier=treating an empty source set as "nobody will ever carry this, retract it" rather than "nobody has carried this" -- removing the ours clause from the pinned projection -- makes an_event_every_host_refused_stays_visible_reporting_zero_relays lose the row from both the already-open feed and a freshly opened one, while its per-host rejection receipts still arrive
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_query_opened_after_the_write_sees_it_exactly_as_one_already_open_does
  # nmp:falsifier=pushing a locally accepted row only to subscriptions that were already open, instead of admitting it to the store every projection reads, makes a_query_opened_after_the_write_sees_it_exactly_as_one_already_open_does see an empty row set from the feed opened after the send and from the second simultaneous feed
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_locally_accepted_write_never_enters_a_query_its_filter_excludes
  # nmp:falsifier=admitting locally accepted rows into a projection without re-checking the query's own filter -- the plausible wrong reading of "show it immediately" -- makes a_locally_accepted_write_never_enters_a_query_its_filter_excludes see the note appear in the unrelated feed through both the delta and the snapshot door
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_write_still_in_flight_is_still_in_the_feed_after_a_restart
  # nmp:falsifier=projecting locally accepted rows from process-local state rather than from the durable store makes a_write_still_in_flight_is_still_in_the_feed_after_a_restart find no row after the redb file is genuinely released and reopened, even though the write is still owed
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-008
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::a_row_no_relay_has_served_is_visible_under_every_pin_and_counts_against_its_bound
  # nmp:evidence=rust:nmp::a_groups_where_listing_never_lets_one_hosts_member_evidence_answer_for_anothers_group
  # nmp:falsifier=widening the ours clause to admit rows this node never wrote under a pin that did not serve them makes a_row_no_relay_has_served_is_visible_under_every_pin_and_counts_against_its_bound return the foreign row it asserts is invisible, and makes a_groups_where_listing_never_lets_one_hosts_member_evidence_answer_for_anothers_group see one host's evidence answer for another host over two live relays
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-009
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::a_row_no_relay_has_served_is_visible_under_every_pin_and_counts_against_its_bound
  # nmp:falsifier=deciding visibility after applying a page bound rather than before it makes a_row_no_relay_has_served_is_visible_under_every_pin_and_counts_against_its_bound see the page under-fill to one row; admitting the locally accepted row without counting it makes the same page over-fill, including through the de-duplicated union door where the bound applies once to the merged set
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-010
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_users_own_row_survives_a_carrier_outside_the_pin_and_reports_it_honestly
  # nmp:evidence=rust:nmp::a_foreign_row_carried_only_outside_the_pin_is_still_invisible
  # nmp:falsifier=restoring the carried-versus-uncarried predicate -- admitting a row under a pin because NO relay has carried it rather than because this node accepted it -- makes the_users_own_row_survives_a_carrier_outside_the_pin_and_reports_it_honestly find the user's message gone from the watched feed the instant the unwatched host carries it, through both the delta and the snapshot door; widening the ours clause to admit every row makes a_foreign_row_carried_only_outside_the_pin_is_still_invisible see somebody else's note answer for a host that never served it
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

  # nmp:id=WRITES-OPTIMISTICPUBLISH-011
  # nmp:status=built
  # nmp:evidence=rust:nmp::optimistic_publication_is_general_and_owes_nothing_to_nip29
  # nmp:evidence=rust:nmp::nip29_code_never_names_publication_visibility_vocabulary
  # nmp:falsifier=implementing optimistic visibility anywhere protocol-specific rather than in the general projection makes optimistic_publication_is_general_and_owes_nothing_to_nip29 fail for a plain note and a plain article, neither of which any group protocol has heard of; reintroducing a protocol-local visibility branch makes nip29_code_never_names_publication_visibility_vocabulary report the provenance or projection identifier that branch cannot be written without
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
