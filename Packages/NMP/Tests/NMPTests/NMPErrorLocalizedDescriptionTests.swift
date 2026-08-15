import XCTest
@testable import NMP

final class NMPErrorLocalizedDescriptionTests: XCTestCase {
    func testEveryCaseHasAStableNativeDescriptionWithItsEvidence() {
        let cases: [(NMPError, String)] = [
            (.nonIndexableFilterTag("sentinel-tag"), #"Not indexable as a filter key: "sentinel-tag""#),
            (.invalidPublicKey("sentinel-pubkey"), #"Invalid public key hex: "sentinel-pubkey""#),
            (.invalidEventId("sentinel-event"), #"Invalid event ID hex: "sentinel-event""#),
            (.invalidRelayUrl("sentinel-relay"), #"Invalid relay URL: "sentinel-relay""#),
            (.invalidTag(["sentinel-name", "sentinel-value"]), #"Invalid tag: ["sentinel-name", "sentinel-value"]"#),
            (.invalidSigner("sentinel-signer"), "Invalid signer: sentinel-signer"),
            (.authCapabilityRegistryFull(limit: 17), "AUTH capability registry is full at 17 entries"),
            (.authCapabilityInstanceExhausted, "AUTH capability instance space exhausted"),
            (.noCurrentSigningProvider, "The current account has no available signing provider"),
            (.invalidSignRequest("sentinel-request"), "Invalid sign request: sentinel-request"),
            (.signerUnavailable("sentinel-unavailable"), "Signer unavailable: sentinel-unavailable"),
            (.signerRejected("sentinel-rejection"), "Signer rejected the request: sentinel-rejection"),
            (.invalidSignerOutput("sentinel-output"), "Invalid signer output: sentinel-output"),
            (.publishRefused("sentinel-refusal"), "sentinel-refusal"),
            (.storeOpenFailed("sentinel-open"), "Could not open store: sentinel-open"),
            (
                .storeAlreadyOpen("/sentinel/store"),
                "Persistent store is already open: /sentinel/store"
            ),
            (
                .storeUnsupportedSchema(path: "/sentinel/store", expected: 13, found: 10),
                "Persistent store /sentinel/store is schema epoch 10, not the one supported epoch 13;"
                    + " it was not migrated, adopted, drained, or reset; discard and recreate this store to continue;"
                    + " NMP can reacquire the relay-backed read cache, but the publish queue state (accepted but"
                    + " unpublished writes, receipts, correlation tokens, route revisions, and attempt evidence) will be"
                    + " permanently lost"
            ),
            (
                .storeUnsupportedSchema(path: "/sentinel/store", expected: 13, found: nil),
                "Persistent store /sentinel/store carries no readable schema marker and is not the one supported"
                    + " epoch 13;"
                    + " it was not migrated, adopted, drained, or reset; discard and recreate this store to continue;"
                    + " NMP can reacquire the relay-backed read cache, but the publish queue state (accepted but"
                    + " unpublished writes, receipts, correlation tokens, route revisions, and attempt evidence) will be"
                    + " permanently lost"
            ),
            (.storeResetFailed("sentinel-reset"), "Could not reset store: sentinel-reset"),
            (.storeStillOpen("/sentinel/store"), "Persistent store is still open: /sentinel/store"),
            (
                .engineStartFailed(component: "sentinel-component", reason: "sentinel-start"),
                "Engine could not start (sentinel-component): sentinel-start"
            ),
            (
                .missingReplaceableCapability(program: Data([1]), format: Data([2])),
                "Store retains replaceable operations for a missing compiled capability"
            ),
            (
                .observationUnavailable(reason: "sentinel-observation"),
                "Observation could not be established: sentinel-observation"
            ),
            (
                .concurrentNext,
                "A next()/signed() call was awaited while a previous one was still in flight; observation streams are single-consumer"
            ),
            (
                .factStreamLagged(receiptId: 23),
                "The finite live fact stream fell behind; reattach receipt 23 to replay"
            ),
            (
                .factStreamLagged(receiptId: nil),
                "The finite live fact stream fell behind before a receipt was observable"
            ),
            (
                .receiptReplayUnavailable(receiptId: 29),
                "Retained evidence for receipt 29 became unavailable during replay"
            ),
            (
                .receiptClosedWithoutOutcome(receiptId: 31),
                "Receipt 31 closed before its terminal outcome"
            ),
            (.signEventAlreadyConsumed, "This sign-event result was already consumed"),
            (.invalidSignature("sentinel-signature"), #"Invalid signature hex: "sentinel-signature""#),
            (.engineClosed, "Engine already shut down"),
            (.invalidNostrEntity("sentinel-entity"), "Invalid Nostr entity: sentinel-entity"),
            (.nostrEntitySecretKeyRejected, "Refusing to decode a secret-key entity"),
            (
                .authorOutboxesRequiresBoundAuthors,
                "SourceAuthority.authorOutboxes requires a selection whose authors field is bound"
            ),
            (.emptyPinnedRelaySet, "SourceAuthority.pinned requires a nonempty relay set"),
            (.windowZeroRows, "Window initial/max must be representable nonzero row counts"),
            (.windowInitialExceedsMax(initial: 31, max: 7), "Window initial 31 exceeds max 7"),
            (.windowSelectionHasLimit, "A windowed selection must not also declare a limit"),
            (
                .relayInformationUnavailable(.http(reason: "sentinel-http")),
                "Relay information unavailable: NIP-11 HTTP request failed: sentinel-http"
            ),
            (
                .invalidCorrelationToken(got: "sentinel-token", reason: "sentinel-correlation"),
                #"Invalid correlation token "sentinel-token": sentinel-correlation"#
            ),
            (.invalidNip73(reason: "sentinel-id"), "Invalid NIP-73 external content id: sentinel-id"),
            (
                .replaceableOperationHasNoWireForm,
                "A registered replaceable operation has no standalone FFI payload"
            ),
        ]

        for (error, expected) in cases {
            XCTAssertEqual(error.errorDescription, expected, "\(error)")
            XCTAssertEqual(error.localizedDescription, expected, "\(error)")
            XCTAssertFalse(error.localizedDescription.contains("NMP.NMPError error"))
        }
    }

    func testReplaceableOperationWireRefusalMapsToThePublicErrorOwner() {
        XCTAssertEqual(
            NMPError(.ReplaceableOperationHasNoWireForm),
            .replaceableOperationHasNoWireForm
        )
    }

    func testEveryRelayInformationKindKeepsItsNestedEvidence() {
        let cases: [(RelayInformationErrorKind, String)] = [
            (.serviceClosed, "Relay information unavailable: NIP-11 acquisition service is closed"),
            (
                .credentialedRelayUrl,
                "Relay information unavailable: NIP-11 acquisition refuses relay URL userinfo"
            ),
            (
                .http(reason: "sentinel-http"),
                "Relay information unavailable: NIP-11 HTTP request failed: sentinel-http"
            ),
            (
                .responseTooLarge(limitBytes: 41),
                "Relay information unavailable: NIP-11 response exceeds 41 bytes"
            ),
            (
                .invalidDocument(reason: "sentinel-document"),
                "Relay information unavailable: invalid NIP-11 document: sentinel-document"
            ),
        ]

        for (kind, expected) in cases {
            XCTAssertEqual(
                NMPError.relayInformationUnavailable(kind).localizedDescription,
                expected
            )
        }
    }
}
