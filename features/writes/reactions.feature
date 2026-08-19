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

    Scenario: A reaction carries the hint, the author slot, the notification and the kind
      Given an event NMP observed at a relay
      When a user reacts to it
      Then the reaction is kind 7 and its reference row carries that relay as its hint
      And the author sits in the reference row's own slot and in a companion "p" row
      And a "k" row names the kind that was reacted to

    Scenario: Reacting to a reply reacts to that reply
      When a user reacts to a note that is itself a reply
      Then exactly one event row is emitted and it names the reply
      And no row carries a thread marker in any position

    Scenario: A reaction notifies the author and nobody the target mentioned
      Given an event that mentions somebody other than its author
      When a user reacts to it
      Then exactly one "p" row is emitted and it names the author
      But replying to the same event carries that mention forward

  Rule: A reaction says one of the three things NIP-25 defines

    Scenario: A reaction is a like, a dislike, or an emoji
      When an app composes each of the three reactions NIP-25 defines
      Then a like renders as "+", a dislike renders as "-", and an emoji renders as itself
      And no caller ever writes the content bytes

    Scenario: An emoji that would say something else refuses before an event exists
      When an app composes an emoji reaction that is empty or a custom-emoji shortcode
      Then the call refuses with a typed error and no draft is produced
      And an ordinary emoji composes unchanged
