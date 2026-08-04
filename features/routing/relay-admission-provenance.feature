Feature: A relay is admitted for whose declaration it is, not what it says
  Admission used to be one rule about addresses: loopback, RFC-1918 and
  `.onion` were refused wherever they came from. That is the wrong question.
  A private address is meaningless -- and possibly hostile -- in somebody
  else's data, and completely ordinary when this app's operator, or the person
  signed into it, declared it, because they are describing their own network.

  The rule is therefore provenance. What this app declared, and what an
  identity it can sign as declared in its own signed relay list, is heeded
  whatever address it names. Everything else still faces the address rule.
  Heeding is permission to try, never a promise the relay works.

  `.onion` is off that axis entirely. It is not a "my network" address, it is a
  reachability question, so it is governed by a capability the app declares
  rather than by a list of local hosts.

  Background:
    Given I am logged in as my own account

  # nmp:id=ROUTING-ADMISSION-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_recipients_localhost_relay_row_is_skipped
  # nmp:falsifier=Apply admission with the author's identity unknown, as a parser must; the recipient's loopback entry is admitted into their inbound routes and this app connects to its own machine believing it is reaching them.
  Scenario: A recipient's relay list naming localhost is skipped
    # Their loopback is not my loopback. Whatever is listening on my port 7777
    # is certainly not the person I am writing to.
    Given someone else's relay list names "ws://127.0.0.1:7777" and a public relay
    When NMP learns their relay list
    Then only the public relay is used to reach them
    And the loopback entry is recorded as refused rather than discarded

  # nmp:id=ROUTING-ADMISSION-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::our_own_localhost_relay_row_is_heeded_however_it_arrived
  # nmp:falsifier=Apply admission at parse time where provenance is unknown; a user whose own relay list names their LAN relay is told they have no relays configured.
  Scenario: My own relay list naming localhost is heeded
    # The bytes came back from an indexer I do not control, over a network I do
    # not trust. What makes the list mine is the key that signed it, and I am
    # the one who signed it.
    Given my own relay list names "ws://127.0.0.1:7777" as my write relay
    When NMP learns my relay list from an indexer
    Then my loopback relay is one of my write destinations
    And the connection to it is attempted rather than refused before dialing

  # nmp:id=ROUTING-ADMISSION-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_identical_list_flips_on_who_signed_it_and_nothing_else
  # nmp:falsifier=Decide admission from how the relay list reached this process rather than from who signed it; a list that arrived over the network is treated as untrusted even when it is my own, so my own relays disappear.
  Scenario: Authorship decides, not how the bytes arrived
    # The same event, the same relay it came from, the same loopback row. The
    # only thing that changes is whether we hold the key that signed it.
    Given a relay list naming "ws://127.0.0.1:7777" arrives from an indexer
    When nobody is signed in as its author
    Then its loopback relay is refused
    When the account that signed it is signed in
    Then the very same list's loopback relay is heeded

  # nmp:id=ROUTING-ADMISSION-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_app_relay_on_localhost_is_heeded_by_the_socket_with_no_allowlist
  # nmp:evidence=rust:nmp-transport::our_own_declaration_reaches_a_local_relay_with_no_allowlist
  # nmp:falsifier=Let the socket re-derive admission from the address instead of carrying the provenance answer routing already gave; the app relay the user was told they have is refused at the dial and can never be reached.
  Scenario: The app's own relay list naming localhost is heeded
    # An operator running a dev relay on their own machine declared it by
    # configuring it. Needing to declare it a second time, in a different
    # setting, meant routing and the socket were answering the same question
    # differently.
    Given the app is configured with "ws://127.0.0.1:7777" as an app relay
    And no local host has been added to the admission allowlist
    Then that relay is heeded when routing a write
    And the socket connects to it rather than refusing it as local
    But a different local address the app never named is still refused

  # nmp:id=ROUTING-ADMISSION-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_relay_hint_naming_a_lan_address_is_skipped_even_when_we_use_that_relay
  # nmp:falsifier=Treat a relay hint as trusted because it names a relay the app already uses; a forged hint gains admission it was never granted.
  Scenario: A relay hint naming a LAN address is skipped
    # A hint lives in someone else's event and says whatever they typed. It
    # carries no authorship at all, which makes it the cheapest thing in Nostr
    # to forge -- so it may not inherit a grant by naming its destination.
    Given the app is configured with "ws://192.168.1.10" as an app relay
    And someone else's event carries a relay hint naming that same address
    When NMP considers where to look for the hinted event
    Then the hint contributes no relay

  # nmp:id=ROUTING-ADMISSION-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::declared_tor_reachability_admits_a_strangers_onion_relay
  # nmp:evidence=rust:nmp-network-policy::onion_is_governed_by_reachability_not_by_the_local_allowlist
  # nmp:falsifier=Keep `.onion` on the local-address axis; an app with Tor available can reach only hidden services it declared itself, and adding one to the local-host allowlist silently grants nothing.
  Scenario: With Tor declared, another person's onion relay is used
    # Reachability is not network ownership. Whether I can reach a hidden
    # service has nothing to do with whose it is.
    Given another person's relay list names an ".onion" relay
    When the app has not declared Tor reachable
    Then that relay is refused, and the reason names reachability rather than a local host
    When the app declares Tor reachable
    Then that same relay is used

  # nmp:id=ROUTING-ADMISSION-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_two_axes_do_not_grant_each_others_addresses
  # nmp:falsifier=Fold the Tor capability into the local-host allowlist; declaring Tor quietly re-admits every stranger's loopback and RFC-1918 relay, which is the exact SSRF pivot the allowlist exists to close.
  Scenario: The two declarations do not grant each other's addresses
    # They answer different questions, so neither may answer the other's.
    Given the app declares Tor reachable but allows no local hosts
    Then a stranger's loopback relay is still refused
    Given the app allows an ".onion" host as a local host but declares no Tor
    Then that hidden service is still refused

  # nmp:id=ROUTING-ADMISSION-008
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_entirely_refused_list_is_not_the_same_value_as_an_empty_one
  # nmp:falsifier=Drop a refused relay instead of retaining it on the author fact; a user with three LAN relays and a user with none become the same value, and the app can only tell both of them they have no relays.
  Scenario: An author whose entire list was refused is not reported as having no relays
    # This is the defect the whole rule exists to close. Both authors end up
    # with nowhere to route, and only one of them should be told they have no
    # relays; the other should be told which relays were turned away and why.
    Given one author's relay list names only addresses on their own network
    And another author's relay list declares no usable relay at all
    When NMP learns both lists
    Then neither author has a routable destination
    But the two are not the same fact
    And the first author's refused relays are named, with the reason they were refused

  # nmp:id=ROUTING-ADMISSION-009
  # nmp:status=built
  # nmp:evidence=rust:nmp::holding_one_keys_signer_never_widens_another_keys_routes
  # nmp:evidence=rust:nmp::an_explicitly_named_identity_is_own_even_when_a_different_key_is_active
  # nmp:falsifier=Heed another key's own-list grant while publishing under `Identity::Explicit`; a write signs as one identity and connects somewhere only a different identity declared.
  Scenario: Own is per identity, not per app
    # An app holding several accounts publishes as a specific one. The question
    # admission asks is whether this is THAT key's own list, never whether some
    # key we hold happens to have said the same thing.
    Given I hold the signing keys for two accounts
    And the first account's relay list names a relay on my own network
    And the second account's relay list names that same address
    When NMP learns the first account's list
    Then that relay is heeded for the first account
    When NMP learns the second account's list while it has no signer
    Then the identical entry is refused for the second account

  # nmp:id=ROUTING-ADMISSION-010
  # nmp:status=built
  # nmp:evidence=rust:nmp::signed_out_leaves_only_the_operator_tier
  # nmp:evidence=rust:nmp::a_detached_signer_stops_being_one_of_our_identities
  # nmp:falsifier=Keep an identity's own-list grant after its signer is detached; a key the app can no longer act as keeps naming local destinations the app will still dial.
  Scenario: Signed out, nothing is mine
    # With no account, there is no "my own network" for anyone to be describing,
    # so only what the app itself declared grants anything.
    Given no account is signed in and no signing key is held
    Then no author's relay list is treated as my own
    But the app's own configured relays are still heeded
    When a signing key is attached and then detached again
    Then that account's relay list stops being treated as my own
