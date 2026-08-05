Feature: A group publication carries the store-issued receipt id
  #1244: `Group::publish` took no correlation and returned no receipt id, so
  a group write was the one write an app could not find again after a crash.
  The stated reason ("every group write reaches the engine's untracked publish
  door, never `publish_tracked`") had been false since the PublishQueue
  rewrite: `Engine::publish` IS `publish_tracked` with the receipt id thrown
  away, so the id was allocated on every group write and discarded on the way
  out.

  `Group::publish` now hands the intent it composes to
  `Engine::publish_tracked` and returns the ordinary `ReceiptStream`,
  store-issued id included -- the same receipt every other write returns.

  #1292 deleted the other half of #1242/#1244's shape. `Group::intent`,
  `Group::signed_intent` and `Group::publish_signed` are gone: no surface
  hands an app an unpublished group intent to stamp a correlation token on,
  and no surface publishes bytes an app signed itself. `Group::publish` is
  the group's only write door, and an app that needs a signed event WITHOUT
  publishing it asks `Engine::sign_event`, which creates no write intent,
  receipt or publication.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  # nmp:id=PROTOCOL-GROUPWRITERECEIPTID-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_hands_back_the_store_issued_receipt_id
  # nmp:evidence=swift:NMP::testAGroupWriteCarriesTheStoreIssuedReceiptID
  # nmp:evidence=kotlin:NMPKotlin::groupWriteFactStreamDeliversOrdinaryWriteFacts
  # nmp:falsifier=Routing the group door back through Engine::publish (the receipt-id-discarding spelling it used before) makes a_group_write_hands_back_the_store_issued_receipt_id lose the id it matches against publish_queue(), and makes the Swift and Kotlin assertions on Receipt.id fail to compile or to find a nonzero id.
  @nip29
  Scenario: A group publication carries the store-issued receipt id
    When I publish an event of kind 9 with content "first light" through the group
    Then the group write carries a store-issued receipt id
    And the published event was delivered to "wss://relay.groups.example"
