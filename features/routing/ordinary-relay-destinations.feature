Feature: Valid relay URLs are ordinary destinations
  NMP does not classify a valid relay destination by whether its host or DNS
  answer is loopback, private, link-local, unspecified, or an onion service.
  Routing preserves declared relays and hints exactly; transport attempts them
  with the platform resolver. Reachability remains an observed connection
  outcome, not a configuration grant or a security verdict.

  # nmp:id=ROUTING-DESTINATIONS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-transport::loopback_connects_without_policy_or_opt_in
  # nmp:evidence=rust:nmp::an_app_declared_loopback_relay_is_reached_without_opt_in
  # nmp:falsifier=Restore a pre-connect address classifier; the real loopback relay is refused before the websocket handshake and never acknowledges the write.
  Scenario: An app relay on loopback is reached without an opt-in
    Given the app is configured with a valid loopback relay URL
    When NMP opens the relay and publishes a write
    Then the transport attempts that destination like any other relay
    And the real loopback relay acknowledges the write

  # nmp:id=ROUTING-DESTINATIONS-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-blossom::loopback_upload_succeeds_without_opt_in
  # nmp:falsifier=Restore the Blossom literal-host gate; authorization validates but the loopback HTTP server observes no upload.
  Scenario: A Blossom server on loopback is an ordinary HTTP target
    Given a valid Blossom server URL names loopback
    When a correctly authorized upload is sent
    Then the default Blossom client reaches the server without an opt-in

  # nmp:id=ROUTING-DESTINATIONS-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::someone_elses_local_relay_list_row_becomes_a_route_candidate
  # nmp:falsifier=Filter kind 10002 rows by address or author ownership; the signed local relay disappears before the router can attempt it.
  Scenario: Another author's local relay remains their route
    Given another author's signed relay list names a valid local relay URL
    When NMP learns that relay list
    Then the local relay remains an exact route candidate
    And routing does not require this app to own the declaration

  # nmp:id=ROUTING-DESTINATIONS-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::nip11_hostname_uses_platform_resolution_without_policy
  # nmp:falsifier=Install a custom DNS resolver in the NIP-11 HTTP client; the iOS Simulator request bypasses platform DNS and the public relay document is unavailable through the supported Swift facade.
  Scenario: NIP-11 uses platform DNS on iOS
    Given a public relay hostname has a NIP-11 document
    When a Swift app asks for relay information on an iOS Simulator
    Then the supported NMP facade returns that public document
    And name resolution is owned by the platform HTTP stack
