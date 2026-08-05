Feature: The NIP-29 write door mints an intent, and the app owns the token on it
  #1242 and #1244 are one defect seen from two sides: the group write door
  handed back something too thin to be useful. It only published, so an app
  whose write path mints intents in one stage and submits them in another
  could not use it at all and hand-assembled the route and identity the door
  exists to own (mosaico's `src/nmp_host/write/compose.rs` says so in its own
  source, citing this issue); and what it handed back carried no receipt id
  and accepted no correlation, so a group write was the one write an app could
  not find again after a crash.

  Both are closed by the same shape: `Group::intent`/`Group::signed_intent`
  are the door, and they PRODUCE the ordinary `WriteIntent` the one publish
  door takes. `Group::publish` is that call plus `Engine::publish_tracked`
  and nothing else, so there is exactly one contextualization and no second
  door. The correlation token is not a parameter on eleven publish verbs: it
  is caller-minted and caller-persisted, so the app stamps its own on the
  minted intent, which is the only place that fact can honestly come from.

  What this deliberately costs is stated in `crates/nmp/src/nip29/group.rs`'s
  own module doc: an app holding a minted intent can read the route and, from
  the payload's `h` row, the group id. The alternative -- a group-shaped
  intent noun only a group-shaped publish door accepts -- is a second write
  lifecycle, which is the thing that module exists not to have.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  # nmp:id=PROTOCOL-WRITEDOORMINTSANINTENT-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_mint_door_hands_back_a_fully_decided_intent_and_publishes_nothing
  # nmp:evidence=rust:nmp-ffi::the_mint_door_projects_an_intent_the_general_publish_door_takes
  # nmp:falsifier=Having Group::intent decide anything less than the whole intent -- omitting the h row, leaving routing Auto, resolving identity later as Identity::Active, or inventing a correlation token -- makes the_mint_door_hands_back_a_fully_decided_intent_and_publishes_nothing fail on the exact field that regressed; having it reach the engine at all makes this scenario's "minting took no receipt and reached no relay" see a receipt or a relay contact.
  @nip29
  Scenario: The mint door decides everything and publishes nothing
    When I mint a group write intent for an event of kind 9 with content "first light"
    Then the minted intent routes explicitly over exactly "wss://relay.groups.example"
    And the minted intent carries an h tag with value "photographers"
    And the minted intent names me as its exact author
    And the minted intent carries no correlation token of its own
    And minting took no receipt and reached no relay

  # nmp:id=PROTOCOL-WRITEDOORMINTSANINTENT-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_is_reattachable_by_the_apps_own_correlation_token
  # nmp:evidence=rust:nmp-ffi::the_mint_door_projects_an_intent_the_general_publish_door_takes
  # nmp:falsifier=Dropping the caller's correlation token anywhere between the minted intent and the store's acceptance transaction makes reattaching by that token find nothing instead of the published receipt; the negative half of the same scenario pins that a token no write carried resolves to nothing, so a lookup that answered with "whatever the last write was" would fail it too.
  @nip29
  Scenario: A group write minted through the door is recovered by the app's own token
    When I mint a group write intent for an event of kind 9 with content "first light"
    And I stamp my own correlation token "room-send-0001" on it and hand it to the one publish door
    Then the group write carries a store-issued receipt id
    And reattaching by correlation token "room-send-0001" recovers that same receipt
    And reattaching by correlation token "room-send-0002" finds nothing

  # nmp:id=PROTOCOL-WRITEDOORMINTSANINTENT-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_inline_door_hands_back_the_store_issued_receipt_id
  # nmp:evidence=swift:NMP::testAGroupWriteCarriesTheStoreIssuedReceiptID
  # nmp:evidence=kotlin:NMPKotlin::groupWriteFactStreamDeliversOrdinaryWriteFacts
  # nmp:falsifier=Routing the inline group door back through Engine::publish (the receipt-id-discarding spelling it used before) makes the_inline_door_hands_back_the_store_issued_receipt_id lose the id it matches against publish_queue(), and makes the Swift and Kotlin assertions on Receipt.id fail to compile or to find a nonzero id.
  @nip29
  Scenario: Even the inline group publication carries the store-issued receipt id
    When I publish an event of kind 9 with content "first light" through the group
    Then the group write carries a store-issued receipt id
    And the published event was delivered to "wss://relay.groups.example"

  # nmp:id=PROTOCOL-WRITEDOORMINTSANINTENT-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_caller_supplied_context_is_refused_before_any_intent_exists
  # nmp:evidence=rust:nmp::the_signed_mint_door_refuses_every_ill_scoped_event_including_a_second_h_row
  # nmp:falsifier=Moving either context refusal downstream of the mint -- so an intent exists before the caller error is decided -- makes a_caller_supplied_context_is_refused_before_any_intent_exists see an Ok instead of the typed refusal; dropping the AmbiguousContext arm makes the_signed_mint_door_refuses_every_ill_scoped_event_including_a_second_h_row accept an event claiming two groups, which is the asymmetric hole a real consumer had left open in its own hand-rolled check.
  @nip29
  Scenario: A caller error is decided at mint time, before any intent exists
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I mint a group write intent for that event
    Then the publication is refused with a typed caller-supplied-h error
    And the error names the h tag
    And minting took no receipt and reached no relay
