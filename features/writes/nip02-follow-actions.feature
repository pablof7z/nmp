Feature: NIP-02 follow actions are durable semantic writes
  An app asks to follow or unfollow a decoded public key. It does not fetch,
  rewrite, or compare an entire kind-3 event. The NIP-02 capability compiles
  that typed action into an ordinary durable semantic operation, and the app
  observes the ordinary receipt rather than a second action lifecycle.

  Scenario: Follow and unfollow use the same ordinary receipt on Rust and native boundaries
    Given a current account whose contact list already follows an unrelated person
    When the app follows a target through the typed NIP-02 action
    Then the target becomes followed without changing the unrelated contact
    And the action exposes the ordinary write receipt facts
    When the app repeats that follow and then unfollows the target
    Then each action remains an ordinary semantic write rather than a separate retry lifecycle
    And direct Rust and the native FFI boundary expose the same relationship and receipt truth

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

  Scenario: An offline follow immediately changes a feed derived from the contact list
    Given the cached contact list follows five authors and their notes are cached
    And the contact-list relay has not answered the current request
    When the app follows a sixth author whose note is cached
    Then the already-open derived feed immediately includes the sixth author's note
    When the engine restarts before the relay provides newer truth
    Then the original receipt reattaches without another follow action
    When later relay truth adds three authors and removes one cached author
    Then the same pending follow is replayed over that relay truth
    And the derived feed keeps the follow, adds those three authors, and removes that one author
    And the original receipt owns the reconciled successor
