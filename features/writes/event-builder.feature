Feature: An event builder demands a kind and permits everything else
  Composing an event today means constructing an unsigned event, and an
  unsigned event requires a pubkey and a created_at before it will exist at
  all. So "publish this note as me" makes an app look up its own public key
  and read a clock before it can even describe what it wants to say -- and
  the engine then refuses to change either, which is the right rule enforced
  against the wrong input. The convenience case pays the whole price of the
  explicit one.

  The builder inverts that. The kind is the one thing NMP cannot invent, so
  the kind is the one thing it demands. The created_at, the pubkey, the id
  and the signature are filled in when the app did not say them.

  "Filled in when absent" is not "not sayable". A builder is not a restricted
  subset of an event and it is not a safety perimeter: an app importing older
  content states its own created_at and keeps it verbatim, an app carrying
  tags NMP has never heard of carries them to the wire unchanged, and a kind
  nobody has written a module for is published rather than refused. NMP fills
  what you left unsaid; it does not overrule what you said, and it does not
  hold a whitelist of what you are allowed to say. An app that wants to
  hand-roll its own gift wrap can, because the alternative -- refusals
  accreting in the one universal type until only blessed shapes get through
  -- costs more than it saves.

  Two fields stay unsayable on a builder, and only two: the id and the
  signature. Both are derived from signed bytes, so both only mean anything
  on a payload that already went through a signer. That is what publishing a
  pre-signed event is for; a builder is by definition the half of the
  lifecycle before the signature.

  What follows from stamping at acceptance is that composing the same logical
  event twice does NOT produce identical bytes. That was raised as a
  requirement and rejected: if reproducible bytes were genuinely required
  they could not be one NIP's concern, they would be every event's, and they
  are enforced nowhere and wanted nowhere. An app that actually wants two
  identical events already has the means -- state the created_at -- and the
  scenarios below pin both halves: NMP does not promise it, and the app can
  still have it.

  Background:
    Given I am logged in as the account with pubkey "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And my relay list names "wss://hub.example" as my write relay

  # ---- what NMP fills in ------------------------------------------------

  @designed
  Scenario: A kind alone is a complete builder
    # The headline, and the whole reason the type exists. Everything an app
    # must say to publish a note is on the first line; the account it
    # publishes as is never spelled out, because the app already told NMP who
    # it was logged in as and repeating that is the ceremony being deleted.
    When I compose an event of kind 1 saying "hello" and publish it
    Then the published event has kind 1
    And the published event is authored by "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    And the published event carries a created_at, an id and a signature
    And I never stated my own pubkey, created_at, id or signature

  @designed
  Scenario: The stamped created_at is the time acceptance happened
    # Not compose time, not the time the relay finally took it. Acceptance is
    # the moment the body is frozen, which is the only moment that is both
    # after the app finished describing the event and before anything
    # downstream depends on the bytes.
    Given my device clock reads "2026-07-29T12:00:00Z"
    When I compose an event of kind 1 saying "hello" and publish it
    Then the published event's created_at is "2026-07-29T12:00:00Z"

  # ---- what the app can still say --------------------------------------

  @designed
  Scenario: An app importing older content states its own created_at and keeps it
    # The import case is the plain one: a note written in 2019 and brought
    # across from somewhere else is not a note written now. If NMP restamped
    # it the import would be a lie, and the app would have no way to say what
    # it means. Present-then-changed must stay impossible.
    Given my device clock reads "2026-07-29T12:00:00Z"
    When I compose an event of kind 1 saying "an old post" created at "2019-03-04T09:15:00Z" and publish it
    Then the published event's created_at is "2019-03-04T09:15:00Z"
    And the published event is authored by "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"

  @designed
  Scenario: Tags NMP has never seen reach the wire unchanged
    # Arbitrary means arbitrary: not reordered, not normalised, not filtered
    # down to the ones a module claims. NMP has no opinion about "imeta" and
    # is not required to have one in order to carry it.
    When I compose an event of kind 1 saying "look at this" with the tags:
      | imeta | url https://example.invalid/x.png | m image/png |
      | client | some-app-nobody-registered      |             |
      | zzz    | a value with spaces             |             |
    And I publish it
    Then the published event carries exactly those tags, in that order, unchanged

  @designed
  Scenario: A kind nobody wrote a module for is published, not refused
    # Guardrails, not restrictions. An app hand-rolling its own gift wrap is
    # allowed to shoot itself in the foot; a builder that validated kinds
    # would make that impossible, and would make every future kind wait on
    # NMP to hear about it first.
    When I compose an event of kind 31337 saying "{}" and publish it
    Then the published event has kind 31337
    And "wss://hub.example" received it
    And nothing refused it for being an unrecognised kind

  # ---- the requirement that was killed ----------------------------------

  @designed
  Scenario: The same logical event composed twice is two valid events
    # A deliberately rejected requirement, recorded here so that nobody
    # reinstates it as a bug. Two composes of "the same" note differ only in
    # the time NMP stamped them, and differing is what timestamps are for.
    # Neither compose is a duplicate of the other and nothing is expected to
    # notice a resemblance.
    When I compose an event of kind 1 saying "hello" and publish it
    And 2 seconds later I compose an event of kind 1 saying "hello" and publish it
    Then both events are accepted
    And the two events differ only in their created_at, id and signature
    And nothing reported either one as a duplicate of the other

  @designed
  Scenario: An app that wants identical bytes states the timestamp itself
    # The other half, and the reason the requirement was safe to kill. Byte
    # reproducibility is an app-level property with an app-level means of
    # getting it. It does not need to be a rule NMP imposes on every event in
    # order to be available to the one app that wants it.
    When I compose an event of kind 1 saying "hello" created at "2019-03-04T09:15:00Z" and publish it
    And 2 seconds later I compose an event of kind 1 saying "hello" created at "2019-03-04T09:15:00Z" and publish it
    Then both events are accepted
    And the two events have the same id
