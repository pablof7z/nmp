Feature: A NIP-02-derived author query follows its contact list
  This is one executable protocol-shaped example of Derived demand. The
  engine is not specialized for notes or contact lists; the fixture proves
  that the app never owns the expanded author set or subscription repair.

  Scenario: Unfollowing one person touches only that person's subscriptions
    Given my relay list names "me-relay" as my write relay
    And Alice's relay list names "alice-relay" as her write relay
    And Bob's relay list names "bob-relay" as his write relay
    And Carol's relay list names "carol-relay" as her write relay
    And Dave's relay list names "dave-relay" as his write relay
    And Alice has posted a note saying "hello from alice"
    And Bob has posted a note saying "hello from bob"
    And Carol has posted a note saying "hello from carol"
    And Dave has posted a note saying "hello from dave"
    And I am logged in as an account that follows Alice, Bob, and Carol
    And my feed of my follows' notes is open
    Then my feed shows Alice's notes
    And my feed shows Bob's notes
    And my feed shows Carol's notes
    When I publish a new follow list with Alice, Bob, and Dave
    Then my feed shows Dave's notes
    And notes from Carol no longer arrive
    And the subscriptions serving Alice and Bob are untouched
