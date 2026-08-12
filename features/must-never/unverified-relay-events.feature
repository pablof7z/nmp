Feature: Relay events are proven before NMP accepts them
  A relay is an untrusted carrier, not an authority over an event's author.
  Before an event can affect the canonical store, routing, acquisition
  evidence, or an app observation, NMP proves that the event id matches its
  body and that the author's Schnorr signature covers that id.

  The proof belongs to the event, not to whichever relay delivered it first.
  Once NMP knows the exact signature for an event id, another relay may reuse
  that proof only when the incoming body still produces that id and its 64
  signature bytes exactly match the known signature. A failed proof is also a
  fact about the exact relay that supplied those bytes; it is never an event
  the app can receive.

  @ledger-5
  # nmp:id=MUSTNEVER-RELAY-EVENT-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-transport::batch_results_match_sequential_verification_and_input_order
  # nmp:evidence=rust:nmp-transport::tampered_event_frame_is_dropped_and_flags_relay_misbehavior
  # nmp:falsifier=Forward an unknown EVENT frame before its id and Schnorr signature are verified; the tampered frame escapes as an ordinary frame instead of producing only relay-misbehavior health.
  Scenario: The first sight of an event is verified before any downstream effect
    Given relay A sends event 1
    And NMP has never verified event 1
    When NMP receives event 1 from relay A
    Then NMP recomputes event 1's id from its body
    And NMP verifies event 1's Schnorr signature
    And both checks finish before event 1 can be stored
    And both checks finish before event 1 can affect routing or acquisition evidence
    And both checks finish before event 1 can reach an app observation

  @ledger-5
  # nmp:id=MUSTNEVER-RELAY-EVENT-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-transport::ordered_cache_policy_rejects_signature_mismatch_and_does_not_poison_on_invalid
  # nmp:evidence=rust:nmp-transport::cached_id_signature_cannot_admit_mutated_event_payload
  # nmp:falsifier=Ignore the verified-event cache when relay B redelivers event 1; the plan no longer selects the known-signature path, or changing the incoming signature bytes is accepted instead of attributed as misbehavior.
  Scenario: Another relay reuses an in-memory proof only for the exact signature
    Given relay A already supplied event 1 during this engine lifetime
    And NMP verified event 1's id and Schnorr signature
    When relay B sends event 1 with the same body and signature
    Then NMP recomputes event 1's id from its body
    And NMP skips Schnorr verification for that redelivery
    And NMP compares all 64 incoming signature bytes with the verified signature
    And event 1 proceeds only because the signatures are byte-for-byte equal

  @ledger-5
  # nmp:id=MUSTNEVER-RELAY-EVENT-003
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1389
  Scenario: A durable verified event avoids repeat Schnorr work after restart
    Given event 1 is already a canonical signed row in NMP's durable store
    And the engine has been reconstructed without its prior in-memory verification cache
    When relay B sends event 1 with the same body and signature
    Then NMP recomputes event 1's id from its body
    And NMP skips Schnorr verification for that redelivery
    And NMP compares all 64 incoming signature bytes with the durable expected signature
    And event 1 proceeds only because the signatures are byte-for-byte equal

  @ledger-5
  # nmp:id=MUSTNEVER-RELAY-EVENT-004
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#755
  Scenario Outline: Invalid event bytes identify the relay that supplied them
    Given NMP receives event 1 from relay B
    And event 1 is invalid because "<failure>"
    When NMP checks event 1 at the relay boundary
    Then event 1 is rejected before storage, routing, acquisition evidence, or app delivery
    And the app can see that relay B supplied an event rejected for cryptographic misbehavior
    And NMP does not turn that fact into an automatic ban or a global relay verdict

    Examples:
      | failure                                                        |
      | its Schnorr signature does not verify                          |
      | its signature bytes differ from the signature known for its id |
