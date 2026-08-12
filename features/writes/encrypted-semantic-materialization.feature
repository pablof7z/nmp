Feature: Private semantic edits wait for exactly the content capability they need
  Accepting a semantic write and constructing complete event bytes are separate
  moments. A public-only edit may be safe without reading an opaque private
  partition. An edit whose meaning depends on private contents waits until the
  exact source can be decrypted and the reconciled contents can be encrypted.

  The user's local NMP database is inside the device trust boundary. It may
  durably retain the plaintext edit instruction needed to honor an offline
  write after restart. Transient decrypted source documents still do not enter
  logs, diagnostics, generic callbacks, or unrelated capability modules.

  Rule: Accepted does not falsely imply body complete

    # nmp:id=WRITES-ENCRYPTED-001
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: A private operation waits without inventing an incomplete Nostr event
      Given a qualified source has an encrypted private partition
      And no matching decrypt capability is currently available
      When the app accepts an operation whose result depends on that private partition
      Then the operation has a durable receipt in the content-pending state
      But it has no materialization event id, signature field, signer request, or relay lane
      And ordinary queries receive no malformed half-event
      When the exact decrypt and encrypt capabilities become available
      Then NMP decrypts the qualified source, applies the operation, and encrypts the reconciled private partition
      And only then does a body-complete signature-pending generation enter ordinary queries

    # nmp:id=WRITES-ENCRYPTED-002
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: A public-only edit preserves opaque private ciphertext
      Given a qualified source has public tags and private ciphertext C1
      And the capability says this public operation does not depend on private contents
      When the app accepts the public operation without a decrypt capability
      Then NMP materializes the public change immediately
      And the materialization contains ciphertext C1 byte-for-byte
      And no decrypt or encrypt request is created

  Rule: Durable operation state and transient source plaintext have different boundaries

    # nmp:id=WRITES-ENCRYPTED-003
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: A private offline operation survives restart in the ordinary local store
      Given a private semantic operation was accepted while its source could not be decrypted
      And its plaintext edit instruction was committed in the user-owned local NMP database
      When the engine closes and reopens without an encryption-at-rest key or vault service
      Then the same receipt and private operation are reconstructed
      And the operation remains content-pending rather than being lost or guessed
      When content capabilities later settle the current source and target
      Then the reconstructed operation materializes exactly once

    # nmp:id=WRITES-ENCRYPTED-004
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: Decrypted source plaintext cannot escape its immediate capability path
      Given a source ciphertext contains a unique sentinel plaintext
      When decrypt succeeds, parsing succeeds or fails, the request is cancelled, the result is stale, the receiver is lost, or the engine shuts down
      Then transient source plaintext is wiped when its one owner is dropped
      And the sentinel is absent from logs, diagnostics, errors, generic app callbacks, and unrelated capability state
      But this does not prohibit the distinct durable semantic edit instruction from the local store

  Rule: Crypto answers are fenced to both source and target

    # nmp:id=WRITES-ENCRYPTED-005
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: A decrypt answer for a superseded source is inert
      Given decrypt request D1 names qualified source S1 and target materialization revision M1
      And source S2 supersedes S1 before D1 completes
      When D1 returns valid plaintext for S1
      Then D1 cannot change the current operation, row, event id, signature work, or relay work
      And reconciliation still requires a decrypt result naming S2 and the current target revision

    # nmp:id=WRITES-ENCRYPTED-006
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: An encrypt answer for a retired target cannot complete its successor
      Given encrypt request X1 names source S1 and target materialization revision M1
      And a newer operation or source creates target revision M2 before X1 completes
      When X1 returns valid ciphertext for M1
      Then M2 does not borrow X1's ciphertext, body-complete state, event id, or signer eligibility
      And M2 still requires an encrypt result naming M2

  Rule: Scheme policy belongs to the capability

    # nmp:id=WRITES-ENCRYPTED-007
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: A conforming list may read legacy NIP-04 and write NIP-44
      Given the owning capability explicitly permits NIP-04 source migration
      And its current write policy requires NIP-44
      When a private edit reconciles a valid NIP-04 source
      Then the source is decrypted as NIP-04
      And the successor private partition is encrypted as NIP-44

    # nmp:id=WRITES-ENCRYPTED-008
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: A capability that negotiates NIP-04 is not silently upgraded
      Given the owning capability selects NIP-04 for its current peer and operation
      When the content materialization is encrypted
      Then NMP requests NIP-04 exactly
      And no global migration policy substitutes NIP-44

  Rule: Content work is bounded and typed

    # nmp:id=WRITES-ENCRYPTED-009
    # nmp:status=specified
    # nmp:gap=implementation
    # nmp:issue=#1382
    Scenario: Oversized encrypted or plaintext content is refused before unbounded work
      Given the capability declares finite ciphertext and plaintext limits
      When a source or crypto result exceeds its corresponding limit
      Then the current request ends with the exact typed limit refusal
      And no oversized buffer is retained, retried, signed, or delivered
