Feature: An app can be the signer, not just hand over a key
  There are two ways an app can give NMP an identity, and for a long time only
  one of them existed. The first is to hand over the secret: NMP holds the key,
  NMP signs. The second is to keep the key and answer questions about it -- the
  app signs, NMP asks. Every signer worth having on a phone is the second kind.
  A Secure Enclave key cannot be handed over; that is the entire point of it. A
  key in the Keychain can be, but an app that surrenders it has thrown away the
  protection it went to the trouble of getting. A remote bunker has no key to
  give. A hardware device has no key to give and a person standing between the
  request and the answer.

  On Swift and Kotlin the second way did not exist at all. Not "was awkward" --
  did not exist. The Rust door for it is generic over a trait whose sign method
  returns a poll-thunk, and neither generics nor thunks cross the FFI boundary,
  so there was nothing an app could call. The consequence was not subtle: the
  one real Swift consumer wrote its user's nsec to a plaintext file in its
  sandbox and shipped two paragraphs in its identity sheet explaining that NMP
  made it do that.

  So this is the second door. It takes no secret and no callback -- only the
  public key the app can sign for -- and hands back a stream of signature
  requests to drain. That shape is not an implementation detail, and the
  reason is not that a person is slow: NMP's capabilities already return
  ready-or-pending, which absorbs human time without holding anything, and the
  AUTH policy does exactly that today. The reason is that NMP must not invoke
  app code at all. The one callback still on this surface is invoked with the
  capability's own mutex held, on a task of the shared runtime, so app code
  that blocks or reenters there freezes work that has nothing to do with it --
  and making that same "NMP calls you" shape safe across the FFI boundary cost
  a five-state hand-written linearization the mailbox needs none of. Inverting
  it means an app that is slow, or wedged, or gone, delays only its own
  signing.

  An app whose drain goes away is the ordinary case, not the pathological one.
  A screen closes, a scope is cancelled, an engine generation is replaced --
  the loop ends and the app is still the signer. So a departing drain must
  never take the signer down with it, and must never leave the mailbox
  unreadable either. Both are ways to silently stop being able to sign.

  What the app cannot do through this door is choose. The key is frozen into
  every request by the write that asked for it, and NMP verifies the returned
  event against the frozen body before it can reach a relay. An app can sign,
  or refuse, or say not right now. It cannot sign as somebody else, and it
  cannot change what it was asked to sign.

  Background:
    Given a key the app can sign for but NMP holds no secret for
    And that key is registered through the app-supplied signer door
    And that key is the active account

  # ---- the door exists at all ---------------------------------------------

  # nmp:id=IDENTITY-APP-SIGNER-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_app_supplied_signer_signs_through_its_mailbox
  # nmp:evidence=rust:nmp-ffi::ffi_an_app_supplied_signer_signs_through_its_mailbox
  # nmp:falsifier=Register the signer through the local-key door instead, which requires the secret; an app that only has a Secure Enclave handle has nothing to pass and the scenario cannot be set up at all.
  Scenario: An app that holds no secret can still produce a signature
    When NMP needs a signature for that key
    Then the request reaches the app
    And the app's signature is accepted
    And the published event is authored by that key

  # nmp:id=IDENTITY-APP-SIGNER-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-ffi::ffi_an_app_supplied_signer_signs_through_its_mailbox
  # nmp:falsifier=Omit the author from the request; an app holding several keys can no longer tell which one this signature is for, and picking wrong is only caught later at the promotion boundary as an opaque rejection.
  Scenario: The request names the key it must be signed by
    When NMP needs a signature for that key
    Then the request states the exact key to sign as
    And it states the exact body to sign

  # ---- refusing ------------------------------------------------------------

  # nmp:id=IDENTITY-APP-SIGNER-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_app_refusal_reaches_the_caller_as_a_rejection
  # nmp:evidence=rust:nmp-ffi::ffi_an_app_refusal_reaches_the_caller
  # nmp:falsifier=Report a decline as unavailable instead; the write parks and waits for a signer that already answered, so a person who said no is asked again forever.
  Scenario: A person declining is a terminal answer, not a retry
    When NMP needs a signature for that key
    And the app reports that the person declined
    Then the caller is told the signer rejected it
    And it is not retried

  # nmp:id=IDENTITY-APP-SIGNER-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_closed_mailbox_parks_writes_instead_of_stranding_them
  # nmp:falsifier=Treat an unavailable signer as a failure; a locked phone in a pocket permanently fails every write instead of holding it until the user unlocks.
  Scenario: A signer that cannot answer right now parks the write
    When the app closes its signer
    And NMP needs a signature for that key
    Then the write parks awaiting a signer
    And it is not refused

  # ---- the app is not trusted to be well-behaved ---------------------------

  # nmp:id=IDENTITY-APP-SIGNER-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_mailbox_the_app_never_reads_does_not_wedge_the_engine
  # nmp:falsifier=Let NMP call the app's signer directly; an app that blocks or never returns now holds the caller for as long as it likes, which is the freeze the pull shape exists to rule out.
  Scenario: A signer nobody is listening to does not wedge the engine
    Given the app registers a signer and never reads its requests
    When NMP needs a signature for that key
    Then the caller is told the signer is unavailable
    And nothing else in the engine is waiting on it

  # nmp:id=IDENTITY-APP-SIGNER-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_undrained_mailbox_refuses_past_its_bound_and_still_works_afterwards
  # nmp:falsifier=Let the queue latch its lag state on overflow; an app that fell briefly behind can never receive another request, so a recoverable backlog silently becomes a permanently dead signer.
  Scenario: An undrained signer saturates and refuses, rather than breaking
    Given the app has let its requests pile up to the bound
    When NMP needs one more signature for that key
    Then that one is refused as unavailable
    And the requests already queued are still delivered
    And answering one makes room for another

  # nmp:id=IDENTITY-APP-SIGNER-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_abandoned_request_answers_unavailable_and_releases_its_slot
  # nmp:falsifier=Drop an unanswered request silently; the write waits forever on an answer nobody is going to give, and the slot it held is gone for the life of the signer.
  Scenario: An abandoned request answers the write instead of stranding it
    Given the app takes a request and then discards it
    Then the write is told the signer is unavailable
    And the room that request occupied is released

  # ---- exactly one answer --------------------------------------------------

  # nmp:id=IDENTITY-APP-SIGNER-008
  # nmp:status=built
  # nmp:evidence=rust:nmp-ffi::ffi_a_request_settles_exactly_once
  # nmp:evidence=rust:nmp::settling_an_unawaited_request_is_reported
  # nmp:falsifier=Allow a second answer to overwrite the first; a write that was already signed and routed can be re-answered afterwards, so what was published and what the app believes it signed diverge.
  Scenario: A request carries exactly one answer
    When the app answers a request
    And the app answers the same request again
    Then the second answer is refused
    And the first answer stands

  # nmp:id=IDENTITY-APP-SIGNER-009
  # nmp:status=built
  # nmp:evidence=rust:nmp-ffi::ffi_a_malformed_answer_does_not_spend_the_request
  # nmp:falsifier=Spend the request on a malformed answer; a single typo in a signature field permanently kills a write the app could have corrected immediately.
  Scenario: A malformed answer is reported without spending the request
    When the app answers with a malformed signature
    Then the app is told the answer could not be parsed
    And the request can still be answered correctly

  # ---- the drain comes and goes; the signer does not ----------------------

  # nmp:id=IDENTITY-APP-SIGNER-011
  # nmp:status=built
  # nmp:evidence=rust:nmp::unparking_ends_the_await_and_leaves_the_signer_working
  # nmp:evidence=rust:nmp-ffi::ffi_unparking_a_drain_frees_the_mailbox_without_closing_it
  # nmp:evidence=swift:NMP::testCancellingADrainEndsItAndLeavesTheSignerWorking
  # nmp:evidence=kotlin:NMPKotlin::cancellingACollectionEndsItAndLeavesTheSignerWorking
  # nmp:falsifier=End the drain by closing the mailbox, the way every other pull handle's teardown does; the first time a screen goes away the app silently stops being a signer and every later write for that key parks forever with nothing to attach.
  Scenario: A drain that goes away does not take the signer with it
    Given the app is waiting for the next request
    When the app's drain is torn down
    Then that wait ends
    And the signer is still registered
    And a replacement drain receives the next request

  # nmp:id=IDENTITY-APP-SIGNER-012
  # nmp:status=built
  # nmp:evidence=rust:nmp::unparking_before_the_await_ends_that_await_and_only_it
  # nmp:evidence=rust:nmp::a_request_that_races_an_unpark_is_retained
  # nmp:evidence=swift:NMP::testCancellingBeforeTheAwaitStillEndsTheDrain
  # nmp:evidence=kotlin:NMPKotlin::unparkEndsOneAwaitAndNotTheMailbox
  # nmp:falsifier=Wake only a reader that is already parked; a drain loop torn down between two requests enters its next wait after the teardown has already run, and waits forever on a mailbox nobody will wake.
  Scenario: A drain torn down before it waits still ends
    Given the app's drain is torn down before it waits again
    When the app waits for the next request
    Then that wait ends immediately
    And a request that arrived meanwhile is still waiting for the next drain

  # ---- it is an ordinary registration -------------------------------------

  # nmp:id=IDENTITY-APP-SIGNER-010
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_stale_registration_cannot_detach_a_replacement_mailbox
  # nmp:evidence=rust:nmp-ffi::ffi_signer_mailbox_registration_is_stale_safe
  # nmp:falsifier=Key removal on the public key rather than the exact installation; cleaning up an old signer silently detaches the one that replaced it, and the app is left believing it still has a working signer.
  Scenario: A stale signer registration cannot detach its replacement
    Given the app registers a second signer for the same key
    When the app removes the first registration
    Then nothing is detached
    And the second signer is still the one NMP asks
