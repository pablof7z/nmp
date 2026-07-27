Feature: A reopened request never inherits a withdrawn one's answer
  Stop caring about something and the relay is told to stop sending it. Start
  caring again and the relay is asked afresh. In between, the answer to the
  first request may still be travelling, and it answers a question nobody is
  asking any more.

  If the second request were reopened under the name the first one used, that
  travelling answer would be indistinguishable from the answer to the second,
  and the query would go on to report that it holds everything the relay was
  asked for — while the relay had not finished sending it. A query must never
  claim it has data that never arrived.

  Scenario: Dropping a follow and adding it back asks the relay afresh
    Given my relay list names "me-relay" as my write relay
    And Alice's relay list names "alice-relay" as her write relay
    And Bob's relay list names "bob-relay" as his write relay
    And Alice has posted a note saying "hello from alice"
    And Bob has posted a note saying "hello from bob"
    And I am logged in as an account that follows Alice and Bob
    And my feed of my follows' notes is open
    Then my feed shows Alice's notes
    And my feed shows Bob's notes
    When I publish a new follow list with Alice
    Then notes from Bob no longer arrive
    When I publish a new follow list with Alice and Bob
    Then relay "bob-relay" never revives a request it was told to stop
    And my feed shows Bob's notes
    And the subscriptions serving Alice are untouched
