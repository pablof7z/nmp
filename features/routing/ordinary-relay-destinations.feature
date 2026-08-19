Feature: Valid relay URLs are ordinary destinations
  NMP does not classify a valid relay destination by whether its host or DNS
  answer is loopback, private, link-local, unspecified, or an onion service.
  Routing preserves declared relays and hints exactly; transport attempts them
  with the platform resolver. Reachability remains an observed connection
  outcome, not a configuration grant or a security verdict.

  Scenario: An app relay on loopback is reached without an opt-in
    Given the app is configured with a valid loopback relay URL
    When NMP opens the relay and publishes a write
    Then the transport attempts that destination like any other relay
    And the real loopback relay acknowledges the write

  Scenario: A Blossom server on loopback is an ordinary HTTP target
    Given a valid Blossom server URL names loopback
    When a correctly authorized upload is sent
    Then the default Blossom client reaches the server without an opt-in

  Scenario: Another author's local relay remains their route
    Given another author's signed relay list names a valid local relay URL
    When NMP learns that relay list
    Then the local relay remains an exact route candidate
    And routing does not require this app to own the declaration

  Scenario: NIP-11 uses platform DNS on iOS
    Given a public relay hostname has a NIP-11 document
    When a Swift app asks for relay information on an iOS Simulator
    Then the supported NMP facade returns that public document
    And name resolution is owned by the platform HTTP stack
