Feature: Current-pubkey-dependent demand reroots
  Scenario: A query using the current pubkey follows the new input
    Given only 1 indexer relay is configured
    And Carol's relay list names "carol-relay" as her write relay
    And Dave's relay list names "dave-relay" as his write relay
    And Carol has posted a note saying "hello from carol"
    And Dave has posted a note saying "hello from dave"
    And I am logged in as an account that follows Carol
    And my feed of my follows' notes is open
    Then my feed shows Carol's notes
    When I switch to a new account that follows Dave
    Then my feed shows Dave's notes
    And notes from Carol no longer arrive

  @wip @ledger-10 @ledger-11
  Scenario: Unrelated multi-account demand survives a current-pubkey change
    Given a live query for kind 9999 p-tagged to every account in this app
    And a separate live query for kind 9999 authored by the current pubkey
    When I change the current pubkey from Alice to Bob
    Then the authored query reroots from Alice to Bob
    And the literal multi-account query remains live and unchanged

  @wip @ledger-10
  Scenario: An accepted write keeps its explicitly selected identity
    Given Alice is the current pubkey
    And I publish an unsigned kind 9999 draft as my podcast identity
    When I change the current pubkey to Bob before signing completes
    Then the pending write still awaits the podcast identity
    And neither Alice nor Bob can sign it
