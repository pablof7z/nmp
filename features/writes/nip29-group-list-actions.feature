Feature: NIP-29 remembered-group actions are durable semantic writes
  An app saves or forgets one exact group identity, or edits the separate
  relay-in-use list. It does not fetch and replace the whole kind:10009 event.
  NMP applies the typed action to current source truth and exposes the same
  ordinary receipt through Rust, FFI, Swift, and Kotlin.

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
