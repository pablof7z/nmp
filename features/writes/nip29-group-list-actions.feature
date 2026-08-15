Feature: NIP-29 remembered-group actions are durable semantic writes
  An app saves or forgets one exact group identity, or edits the separate
  relay-in-use list. It does not fetch and replace the whole kind:10009 event.
  NMP applies the typed action to current source truth and exposes the same
  ordinary receipt through Rust, FFI, Swift, and Kotlin.

  # nmp:id=PROTOCOL-NIP29-GROUP-LIST-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::group_and_relay_operations_touch_only_their_exact_valid_tags
  # nmp:evidence=parity:nmp-parity::direct_and_ffi_group_list_actions_are_identical_and_host_is_not_route
  # nmp:evidence=swift:NMP::testTypedGroupListActionReturnsTheOrdinaryReceipt
  # nmp:evidence=kotlin:NMPKotlin::typedGroupListActionReturnsTheOrdinaryReceipt
  # nmp:falsifier=Treat a group host from event data as a publish destination, or let a group operation edit relay-in-use tags; the direct/FFI route-and-value parity witness observes the extra host delivery or changed unowned row.
  Scenario: Typed group and relay actions own only their exact public rows
    Given my group list contains unrelated rows, duplicate valid rows, malformed rows, and private content
    When I add or remove one exact group id and canonical host identity
    Then every valid exact duplicate follows the requested group action
    And same-id groups on another host, malformed evidence, unrelated order, and content bytes survive
    And no relay-in-use row changes
    When I add or remove one relay-in-use value
    Then no group row changes
    And the group host carried inside the event never becomes a publication destination
    And Rust and the native boundary return the ordinary receipt

  # nmp:id=PROTOCOL-NIP29-GROUP-LIST-002
  # nmp:status=built
  # nmp:evidence=parity:nmp-parity::first_group_list_action_survives_restart_and_replays_over_later_truth
  # nmp:falsifier=Register the NIP-29 materializer only when an action method is called; after restart the retained first-value operation cannot reapply over a later relay list, so the successor or original receipt assertion fails.
  Scenario: A first saved group resumes after restart and rebases over later list truth
    Given no relay kind:10009 value is available for the current account
    When the app saves one group
    Then NIP-29 creates one complete pending kind:10009 through the ordinary receipt
    And the durable operation and receipt survive engine restart
    When the author's outbox later supplies a newer group list
    Then native engine construction has already restored the NIP-29 materializer
    And NMP reapplies the saved group over that relay value without another app action
    And relay-owned groups, relay-in-use rows, malformed rows, order, and content bytes survive
    And the successor remains owned by the original receipt
