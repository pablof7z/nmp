Feature: An already-signed event is published exactly as it arrived
  Some events are not composed here. A user sees a note from somebody they
  follow, right-clicks it, and sends that exact event -- signed by its own
  author, id and all -- to their personal archive relay. Nothing about that
  write is the user's to change. Restamping it would break the signature,
  reordering its tags would break the id, and re-signing it would make it a
  different event by a different person.

  So a signed payload is carried, not composed. It is verified as it stands,
  never handed to a signer, and put on the wire byte for byte.

  That makes it the one payload that STATES its author, which is what makes
  identity mean something different here. Everywhere else, naming an identity
  selects the author, because there is no author until something does. Here
  the author is already frozen in the bytes, so naming an identity can only
  agree with it. Naming the event's own author is a harmless restatement of
  consent. Naming anybody else is a contradiction with no correct resolution
  -- NMP cannot honour both, and picking either one silently would be worse
  than refusing -- so it fails closed before acceptance.

  The rule generalises, and any future payload has to land on one side of it
  deliberately: where an author is absent, identity selects; where an author
  is stated, identity may only restate.

  Background:
    Given "4c26d9074c27d89ede59270c0ac14b71e071b15239519f75474b2f3ba63481f5" has posted a note saying "a note worth keeping"
    And that note is the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92"

  # ---- carried, not composed ---------------------------------------------

  Scenario: A signed event reaches the relay byte for byte
    # The whole contract in one scenario. Every field the builder path would
    # have filled in is already filled in, and every one of them is left
    # alone.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the active account
    When I publish the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92" as-is to "wss://archive.example"
    Then "wss://archive.example" received exactly the bytes I handed over
    And the event it received still has id "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92"
    And the event it received is still authored by "4c26d9074c27d89ede59270c0ac14b71e071b15239519f75474b2f3ba63481f5"
    And its created_at, tags and signature are the ones it arrived with
    And no signer was asked for anything

  Scenario: Publishing someone else's signed event needs no session at all
    # A signed event needs no signer, so it needs no active account, so being
    # logged out is not a reason to refuse it. This is the archive case in
    # its plainest form: no identity is involved anywhere in this write.
    Given no account is active
    When I publish the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92" as-is to "wss://archive.example"
    Then "wss://archive.example" received it unchanged
    And nothing was refused for want of an active account

  Scenario: An event that does not verify is refused before acceptance
    # Verified verbatim means verified. Carrying bytes without checking them
    # would make the archive path a way to launder a forgery through NMP's
    # own publish door.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the active account
    And the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92" has had one byte of its content altered
    When I publish it as-is to "wss://archive.example"
    Then the write is refused for failing verification
    And it never reports accepted
    And "wss://archive.example" received nothing

  # ---- what identity may say about it ------------------------------------

  Scenario: Naming the signed event's own author is a harmless restatement
    # The app saying out loud what the bytes already say. It agrees, so it
    # changes nothing -- including not turning the write into something that
    # needs that key's signer, because the event is already signed.
    Given no account is active
    When I publish the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92" to "wss://archive.example" naming identity "4c26d9074c27d89ede59270c0ac14b71e071b15239519f75474b2f3ba63481f5"
    Then "wss://archive.example" received it unchanged
    And no signer was asked for anything

  Scenario: Naming an identity the signed event disagrees with is refused
    # The one place a comparison still has two operands, and the one place
    # the fail-closed check survives as a check rather than as structure.
    # There is no resolution that honours both statements: restamping the
    # author would invalidate the signature, and ignoring the named identity
    # would publish under a key the app did not consent to.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the active account
    When I publish the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92" to "wss://archive.example" naming identity "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"
    Then the write is refused as a consent and author contradiction
    And it never reports accepted
    And no journal row was written and no write id was allocated
    And "wss://archive.example" received nothing
    And the event was not re-signed as "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90"

  Scenario: Naming no identity does not silently mean the active account
    # The trap this rules out. If "no identity named" resolved to the active
    # account the way it does for a builder, publishing a followee's note
    # while logged in would either be refused as a mismatch or -- far worse
    # -- attributed to me. It means neither: it means the event's own author,
    # whoever that is.
    Given "2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90" is the active account
    When I publish the signed event "82b0093ed2cb1d1520eb14289c8b5b91a8b718dea03fe169dbec760ae4207d92" as-is to "wss://archive.example"
    Then "wss://archive.example" received it unchanged
    And the event it received is still authored by "4c26d9074c27d89ede59270c0ac14b71e071b15239519f75474b2f3ba63481f5"
    And the write was not refused
