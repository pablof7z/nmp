Feature: NIP-02 follow actions are durable semantic writes
  An app asks to follow or unfollow a decoded public key. It does not fetch,
  rewrite, or compare an entire kind-3 event. The NIP-02 capability compiles
  that typed action into an ordinary durable semantic operation, and the app
  observes the ordinary receipt rather than a second action lifecycle.

  # nmp:id=PROTOCOL-NIP02-FOLLOW-001
  # nmp:status=built
  # nmp:evidence=parity:nmp-parity::direct_and_ffi_follow_actions_are_identical_over_real_loopback
  # nmp:falsifier=Replace the FFI action's ordinary receipt projection with a component-owned failure or completion; direct and FFI relationship snapshots and ordered receipt facts diverge.
  Scenario: Follow and unfollow use the same ordinary receipt on Rust and native boundaries
    Given a current account whose contact list already follows an unrelated person
    When the app follows a target through the typed NIP-02 action
    Then the target becomes followed without changing the unrelated contact
    And the action exposes the ordinary write receipt facts
    When the app repeats that follow and then unfollows the target
    Then each action remains an ordinary semantic write rather than a separate retry lifecycle
    And direct Rust and the native FFI boundary expose the same relationship and receipt truth

  # nmp:id=PROTOCOL-NIP02-FOLLOW-002
  # nmp:status=built
  # nmp:evidence=parity:nmp-parity::first_follow_survives_restart_and_replays_over_later_nip02_truth
  # nmp:falsifier=Install NIP-02 lazily only when follow or unfollow is called; after restart the retained first-value operation has no capability to reapply it over later relay truth.
  Scenario: A first follow resumes after restart and rebases over later relay truth
    Given no relay contact list is available for the current account
    When the app follows a target
    Then NIP-02 creates one complete pending kind-3 through the ordinary receipt
    And the durable operation and receipt survive engine restart
    When a relay later supplies a newer contact list
    Then native engine construction has already restored the NIP-02 capability
    And NMP reapplies the retained follow over that relay value without another app action
    And relay-owned content, contacts, hints, petnames, and unrelated tags survive
    And the successor remains owned by the original receipt
