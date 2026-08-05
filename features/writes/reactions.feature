Feature: A reaction is a schema NMP owns, not four rows an app assembles
  A reaction is "a kind 7 event that is used to indicate user reactions to other
  events". NMP had no door for it at all, so both consuming apps wrote one by
  hand: kind 7, an "e" row, a "p" row, a content string. Four lines, and every
  one of them is a decision the app should never have been making.

  What that spelling loses is not the kind. It is everything the one tagging
  door already fills for every other pointer in NMP: the relay hint NMP actually
  observed the event at, the author in the reference row's own slot, and the "k"
  row naming what was reacted to. All three were simply absent from the
  hand-written pair, and absent is invisible -- nothing the app can see is
  missing.

  It also loses the thing that is actually wrong rather than merely thin. NIP-25
  says there MUST always be an "e" tag set to the id of the event being reacted
  to. An app that reaches for its threading helper gets the conversation's root
  emitted first, and a client that tallies reactions by the first "e" then
  credits the root with a reaction nobody gave it. That is the same defect a
  threaded repost has, for the same reason, and it is why a reaction names an
  ENTITY.

  And the content is not a free string. NIP-25 assigns fixed meanings to fixed
  bytes: "+" or the empty string MUST be read as a like, "-" MUST be read as a
  dislike, and an emoji SHOULD NOT be read as either. An app that passes content
  by hand can spell "like" three ways, and can spell it by accident the moment
  an emoji picker returns nothing.

  Rule: A reaction points at an entity and carries what the library knows

    # nmp:id=WRITES-REACTIONS-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-nip25::a_reaction_carries_the_hint_the_author_slot_the_p_row_and_the_k_row
    # nmp:evidence=rust:nmp-ffi::a_native_reaction_is_kind_7_and_carries_what_the_one_door_fills
    # nmp:evidence=swift:NMP::testReactionIsKindSevenAndCarriesWhatTheOneDoorFills
    # nmp:evidence=kotlin:NMPKotlin::reactionIsKindSevenAndCarriesWhatTheOneDoorFills
    # nmp:falsifier=Let the reaction build its own rows instead of taking them from the one tagging door; the relay hint, the author slot and the "k" row go missing exactly as they did in both apps' hand-written pairs, and nothing visible to the composing app is absent.
    Scenario: A reaction carries the hint, the author slot, the notification and the kind
      Given an event NMP observed at a relay
      When a user reacts to it
      Then the reaction is kind 7 and its reference row carries that relay as its hint
      And the author sits in the reference row's own slot and in a companion "p" row
      And a "k" row names the kind that was reacted to

    # nmp:id=WRITES-REACTIONS-002
    # nmp:status=built
    # nmp:evidence=rust:nmp-nip25::reacting_to_a_reply_names_the_reply_and_never_its_root
    # nmp:evidence=rust:nmp-ffi::a_native_reaction_to_a_reply_names_the_reply_and_never_its_root
    # nmp:evidence=swift:NMP::testReactingToAReplyNamesTheReplyAndNeverItsRoot
    # nmp:evidence=kotlin:NMPKotlin::reactingToAReplyNamesTheReplyAndNeverItsRoot
    # nmp:falsifier=Thread a reaction's rows the way a reply threads them; reacting to a reply then emits the thread root's "e" row first, and a client tallying reactions by the first "e" credits the root with a reaction nobody gave it.
    Scenario: Reacting to a reply reacts to that reply
      When a user reacts to a note that is itself a reply
      Then exactly one event row is emitted and it names the reply
      And no row carries a thread marker in any position

    # nmp:id=WRITES-REACTIONS-003
    # nmp:status=built
    # nmp:evidence=rust:nmp-nip25::a_reaction_notifies_the_author_and_nobody_the_target_mentioned
    # nmp:falsifier=Carry the target's own "p" rows forward into a reaction the way a reply carries them; everybody the target mentioned is notified of a reaction they were not part of, which NIP-25 explicitly does not recommend and NIP-10 explicitly does require of a reply.
    Scenario: A reaction notifies the author and nobody the target mentioned
      Given an event that mentions somebody other than its author
      When a user reacts to it
      Then exactly one "p" row is emitted and it names the author
      But replying to the same event carries that mention forward

  Rule: A reaction says one of the three things NIP-25 defines

    # nmp:id=WRITES-REACTIONS-004
    # nmp:status=built
    # nmp:evidence=rust:nmp-nip25::the_three_readings_nip25_defines_render_as_plus_minus_and_the_emoji
    # nmp:evidence=rust:nmp-ffi::the_native_reaction_vocabulary_is_nip25s_three_readings
    # nmp:evidence=swift:NMP::testTheReactionVocabularyIsNip25sThreeReadings
    # nmp:evidence=kotlin:NMPKotlin::theReactionVocabularyIsNip25sThreeReadings
    # nmp:falsifier=Let a caller pass the reaction's content as a string; "like" becomes spellable three ways, and an emoji picker that returns nothing publishes an upvote nobody asked for, because NIP-25 reads the empty string as "+".
    Scenario: A reaction is a like, a dislike, or an emoji
      When an app composes each of the three reactions NIP-25 defines
      Then a like renders as "+", a dislike renders as "-", and an emoji renders as itself
      And no caller ever writes the content bytes

    # nmp:id=WRITES-REACTIONS-005
    # nmp:status=built
    # nmp:evidence=rust:nmp-nip25::an_empty_emoji_refuses_rather_than_silently_becoming_a_like
    # nmp:evidence=rust:nmp-nip25::a_custom_emoji_shortcode_refuses_because_its_companion_row_is_not_written
    # nmp:evidence=rust:nmp-ffi::an_emoji_that_would_say_something_else_refuses_before_a_builder_exists
    # nmp:evidence=swift:NMP::testAnEmojiThatWouldSaySomethingElseRefuses
    # nmp:evidence=kotlin:NMPKotlin::anEmojiThatWouldSaySomethingElseRefuses
    # nmp:falsifier=Accept any string as an emoji reaction; an empty one publishes a like, and a ":shortcode:" publishes literal colons because the NIP-30 "emoji" row that resolves it is not written here -- the same half-formed reference the deleted quote-shaped chat reply shipped.
    Scenario: An emoji that would say something else refuses before an event exists
      When an app composes an emoji reaction that is empty or a custom-emoji shortcode
      Then the call refuses with a typed error and no draft is produced
      And an ordinary emoji composes unchanged
