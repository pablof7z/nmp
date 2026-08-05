Feature: Pointing at something is one door, and it fills what the library knows
  Every reference row on Nostr says the same thing -- go look at this, here is
  where, and here is who wrote it -- and until now every composer wrote those
  rows by hand. All 33 of them across nmp, mosaico and 29er-next were counted.
  The relay hint was filled in one. The author cell was filled in one. A NIP-10
  marker appeared in one, and that one was Swift. So hints were not sometimes
  wrong: nothing in the tree emitted one, and one hand-built row was wrong in a
  way nothing could catch -- a kind:9 chat reply pointing with `q`, which is
  NIP-18's QUOTE marker whose entire stated purpose is keeping the referenced
  event OUT of the thread.

  The door takes the thing being pointed at and nothing else. Not a
  relationship, not a marker, not a hint, not an author -- those are what it
  fills, from the target's own rows and from the relays NMP actually saw the
  event at.

  What it reads is the target's own thread position, never the kind being
  composed. Every library that took the relationship as a parameter instead
  shipped a bug for it: amethyst#629 marked a direct reply-to-root "reply"
  rather than "root" and broke thread reconstruction five hops deep, and NDK's
  two reply paths disagree with each other about the same operation. An app
  cannot get the root wrong here because an app is never asked.

  Reading has to be generous where writing is strict. Four wire shapes mean
  "this is a reply", and a fifth -- applesauce's two rows carrying one id,
  marked "root" then "reply" -- is emitted by a shipping library and locked in
  by its own test. All of them must read to the same place. NMP still writes
  only one of them: NIP-10 says a direct reply to a root carries a single
  marked `root` row, twice, its own git history converged on that deliberately,
  and rust-nostr deleted the double-marked form as redundant.

  What deliberately does NOT come through this door: a NIP-29 roster row, where
  index 2 is a role and not a relay; a NIP-51 list entry; a NIP-9 deletion
  target. None of them is pointing at something for a reader to go and follow,
  so a relay hint would be meaningless in all three.

  Rule: The target's own thread position decides the rows, not the caller

    # nmp:id=WRITES-TAGGING-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::same_target_yields_same_rows_regardless_of_caller
    # nmp:evidence=rust:nmp-grammar::a_root_is_tagged_with_a_single_root_marked_row
    # nmp:falsifier=Make the door read the composing kind, or let a caller state root-vs-reply; tagging one reply from a reply composer and from a reaction composer must then differ, or a reply to a reply must name the target as its own root.
    Scenario: The same target tagged from different composers yields the same rows
      Given an event that is itself a reply to a thread root
      When one app tags it while composing a reply and another tags it while composing a reaction
      Then both emit the thread's root as root and the target as the reply
      And the two sets of pointer rows are byte-identical

    # nmp:id=WRITES-TAGGING-002
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::every_wire_reply_shape_reads_to_the_same_thread_position
    # nmp:falsifier=Drop the no-root-marker case, or treat applesauce's duplicate-id pair as malformed; a reply written by current rust-nostr or by applesauce then reads to a different thread position than the identical reply written by NMP.
    Scenario: Every wire shape that means one thread position reads to that position
      Given replies written as a marked root-and-reply pair, as positional rows, with only a "reply" marker, and as applesauce's two rows carrying one id
      When NMP reads each one's thread position
      Then every shape yields the same root and the same parent
      And a direct reply to a root names no separate parent in any of them

  Rule: The row carries what the library already knows

    # nmp:id=WRITES-TAGGING-003
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::every_pointer_emits_its_author_row_unless_declined
    # nmp:evidence=rust:nmp-ffi::a_native_chat_reply_is_kind_9_and_points_with_e
    # nmp:falsifier=Stop emitting the companion p row, or drop the author from the reference row's own slot when the p row is declined; the parent author stops being notified with nothing visibly missing, which is the bug quartz shipped.
    Scenario: A pointer carries its author, its hint, and its companion notification
      Given an event NMP observed at a relay
      When an app points at it
      Then the reference row carries that relay as its hint and the author in its own slot
      And a companion "p" row names the author
      But declining the author row removes only the "p" row, never the author from the reference row

    # nmp:id=WRITES-TAGGING-004
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::carry_forward_and_dedup_behave_identically_on_every_path
    # nmp:evidence=rust:nmp-grammar::modifiers_compose_in_any_order
    # nmp:falsifier=Let one internal path dedupe and another not, or strip the composing account's own "p" row automatically; a duplicate on the wire becomes a duplicate here, or an agent adding itself to a group publishes an operation naming nobody.
    Scenario: Carry-forward differs per relationship and dedupes the same way everywhere
      Given an event that mentions one person twice and the composing account once
      When an app replies to it
      Then the reply carries the parent's mentions forward exactly once each
      But a reaction to the same event notifies only its author
      And excluding the composing account is something the caller says, never something signing does

  Rule: NIP-22 states importance with case, never with a marker

    # nmp:id=WRITES-TAGGING-005
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::nip22_root_scope_is_uppercase_with_no_marker_slot
    # nmp:evidence=rust:nmp-nip22::replying_to_a_comment_keeps_the_root_the_wire_states
    # nmp:falsifier=Put a "root" or "reply" marker on a comment's rows, or let the caller restate the root while replying to a comment; the first is the mistake NDK shipped and reverted, and the second lets an app pin a reply to a thread it is not in.
    Scenario: A comment's root scope is uppercase and unmarked
      When an app comments on an article
      Then the comment's root rows are uppercase and carry no marker in any position
      And replying to that comment keeps the root its own rows state

  Rule: A schema with its own reply convention offers its own verb

    # nmp:id=WRITES-TAGGING-006
    # nmp:status=built
    # nmp:evidence=rust:nmp-nipc7::chat_reply_points_with_e_and_never_q_or_h_or_previous_rows
    # nmp:evidence=swift:NMP::TaggingTests.testChatReplyIsKindNineAndPointsWithE
    # nmp:evidence=kotlin:NMPKotlin::TaggingTest.chatReplyIsKindNineAndPointsWithE
    # nmp:falsifier=Route kind:9 replies through the general reply verb, or restore the "q" reply row; a group chat reply becomes a kind 1111 no NIP-29 client will ever fetch, or points with the marker that keeps it out of its own thread.
    Scenario: A group chat reply stays kind 9 and points with "e"
      When a chat app replies to a message in a group
      Then the reply is kind 9 and points with an "e" row
      And it carries no "q" row, no group context row, and no timeline evidence
      And a native app composes it through NMP rather than hand-building the row

  Rule: A reference written into content cannot disagree with its row

    # nmp:id=WRITES-TAGGING-007
    # nmp:status=built
    # nmp:evidence=rust:nmp-grammar::an_inline_reference_and_its_row_come_from_one_statement
    # nmp:evidence=rust:nmp-grammar::interpolated_rows_never_disturb_the_rows_a_composer_stated
    # nmp:falsifier=Let content be written separately from the rows its references need; a quote row appears with nothing in the content quoting it, which is what made the old chat reply unrenderable, or a mention appears in content with no "p" row to notify anyone.
    Scenario: Naming someone inside a message emits the row that resolves them
      When an app writes a message naming a person and an event inline
      Then the rendered content carries their bech32 forms and the event's quote row is emitted
      And the rows a composer stated for its own reasons are untouched

  Rule: A repost points at an entity, not at a position in a conversation

    # nmp:id=WRITES-TAGGING-008
    # nmp:status=built
    # nmp:evidence=rust:nmp-nip18::reposting_a_reply_names_the_reply_and_never_its_root
    # nmp:evidence=rust:nmp-nip18::a_text_note_reposts_as_kind_6_and_anything_else_as_kind_16_plus_k
    # nmp:falsifier=Thread a repost's rows; a reposted reply then emits the root's "e" row first, and a NIP-18 reader takes the first "e" as the reposted event, so the user reposts a note they never chose.
    Scenario: Reposting a reply reposts that reply
      When a user reposts a note that is itself a reply
      Then exactly one event row is emitted and it names the reply
      And a reposted text note becomes kind 6 while anything else becomes kind 16 stating what it reposted
