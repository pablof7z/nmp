Feature: A registered replaceable operation is derived once, at acceptance, from whatever source NMP currently holds
  A capability-owned operation names a change, not a byte-for-byte event: "follow
  Alice", not "publish this exact kind:3". The configured capability materializes
  the complete replacement synchronously, against the best source NMP currently
  has -- offline, that is the capability's own first-value policy; once a newer
  relay source arrives, NMP re-runs the same materializer over it and installs a
  successor generation, preserving every still-open operation's receipt identity
  across the replacement.

  This removes the seam a caller-composed replacement cannot close on its own:
  no read/compose/publish loop can pick a correct timestamp or base when a newer
  source can arrive between any two of its own steps, because the timestamp and
  the source it is measured against are decided together, inside the one
  transaction that installs the generation.

  Background:
    Given I am logged in as the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And my relay list names "wss://hub.example" as my write relay
    And my contact list "3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe" created at "2026-07-29T12:00:00Z" is the stored winner

  # ---- complete before custody ------------------------------------------

  # A capability operation is accepted only after that capability has
  # produced the whole unsigned replacement. NMP does not accept a promise to
  # fill in content, tags, or an event id later. Failure to produce the whole
  # event is therefore a refusal before custody, not a parked write.

  Scenario: An accepted capability operation already has one complete replacement event
    Given I am disconnected from every relay
    When a configured capability adds Alice to my contact list
    Then the write is accepted through the ordinary write-intent lifecycle
    And its ordinary receipt names the complete replacement event
    And the complete signature-pending replacement is the current live-query value
    And no accepted state is waiting for content, tags, or an event id

  Scenario: An unavailable capability refuses the operation before custody
    Given the capability required by the operation is not configured
    When I try to add Alice to my contact list through that capability
    Then publishing is refused with a typed configuration error
    And NMP retains no receipt, write intent, optimistic row, signing work, route, delivery work, or correlation

  @acceptance
  Scenario: A trusted capability edit runs without starting another thread
    Given a compiled contact-list capability is supplied when NMP starts
    When I follow Alice while offline
    And a newer remote contact list later arrives
    Then the complete replacement is visible immediately
    And repeated initial and successor edits do not change the process thread count

  @acceptance
  Scenario: Reopening retained work without its compiled capability fails at the door
    Given I accepted a follow while offline
    And I close NMP
    When I reopen the same store without that compiled capability
    Then construction is refused
    And the store is unchanged

  # The encrypted content is opaque to an operation that owns only a public
  # tag. Its presence does not turn that operation into a crypto operation.

  Scenario: A public tag-only edit preserves opaque encrypted content without crypto
    Given my stored contact list contains opaque encrypted content
    And no decryption capability is available
    When a configured capability adds Alice as a public contact tag
    Then the write is accepted
    And the replacement preserves the encrypted content byte for byte
    And NMP does not request decryption or encryption

  # This is the contrast with the preceding scenario: crypto is required by
  # what the operation asks to change, not merely by encrypted bytes being
  # present elsewhere in the event.

  Scenario: An encrypted-content edit without its required crypto refuses before custody
    Given my stored contact list contains opaque encrypted content
    And the requested operation must decrypt and rewrite that content
    And the required crypto capability is unavailable
    When I try to publish that operation
    Then publishing is refused with a typed crypto-capability error
    And NMP retains no receipt, write intent, optimistic row, signing work, route, delivery work, or correlation

  Scenario: Several offline operations keep their receipts while sharing one complete current event
    Given I am disconnected from every relay
    When a configured capability adds Alice to my contact list
    And the configured capability then adds Bob to my contact list
    Then both operations have distinct ordinary receipts
    And one complete current signature-pending event contains Alice and Bob
    And both receipts name that current event without creating another receipt lifecycle

  Scenario: A newer relay version is combined with every active local operation
    Given Alice and Bob were added to my contact list while offline
    And a relay later supplies a newer contact list containing Carol
    When NMP applies the configured contact-list capability to that newer version
    Then one complete current replacement contains Alice, Bob, and Carol
    And its timestamp is exactly one second after the newer relay version
    And the live query moves directly from the prior complete replacement to the successor
    And the raw relay version is never the effective live-query value
    And the original operation receipts now name the successor as current

  Scenario: Source and successor recover as one durable state
    Given active contact-list operations have one complete current replacement over B0
    And a relay supplies newer source B5
    When NMP crashes while replacing the current event with the successor over B5
    Then reopen recovers either the complete B0 generation or the complete B5 generation
    And it never recovers raw B5 as the effective value
    And every original receipt names the same recovered current generation

  Scenario: A successor retires predecessor work and republishes to every destination
    Given relay 1 received current generation E1
    And relay 2 later supplies a newer source version
    When NMP creates successor generation E2
    Then E1 signer, handoff, acknowledgement, timeout, authentication, and retry completions cannot advance E2 or put E1 back on the wire
    And E2 has fresh delivery work for relay 1 and relay 2
    And after restart only E2 resumes active delivery
    And E1 delivery evidence remains historical evidence naming E1

  Scenario: Shared operation receipts observe one physical generation delivery
    Given Alice and Bob have distinct operation receipts sharing current generation E2
    And their destination plans overlap
    When E2 is signed and delivered
    Then exactly one signer request and one physical publication per relay occur for E2
    And both receipts expose signing and relay evidence naming E2

  Scenario: An unreachable destination keeps a semantic operation open
    Given one routed destination for the current semantic generation is unreachable
    When every other destination becomes terminal
    Then each operation receipt remains open with event-qualified terminal relay evidence
    And a later qualified source may still create one successor generation
    And no terminal receipt is resurrected

  @acceptance
  Scenario: A semantic operation settles once routing is closed and every lane is terminal
    Given a follow is routed to its destinations
    When every lane of the current generation becomes terminal
    Then every contributing operation receipt settles atomically
    And the durable semantic resource and its replay program are removed
    And a later unrelated list does not recreate the action

  @acceptance
  Scenario: A later destination receives the same signed generation without resending completed destinations
    Given semantic generation E2 is signed and relay A has accepted it
    And E2 is still waiting for one recipient's relay list
    When that relay list adds relay B as a destination
    Then relay B receives the exact same E2 event id and signature once
    And relay A does not receive E2 again

  @acceptance
  Scenario: A signed successor survives predecessor session replacement
    Given relay A and relay B accepted terminal generation E1
    And relay B supplies a newer source that creates signed generation E2
    When predecessor write sessions are replaced by the current E2 generation
    Then relay A and relay B each receive E2 exactly once
