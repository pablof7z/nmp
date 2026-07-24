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
            (.invalidSecretKey, "Invalid secret key"),
            (.invalidSigner("sentinel-signer"), "Invalid signer: sentinel-signer"),
            (.authCapabilityRegistryFull(limit: 17), "AUTH capability registry is full at 17 entries"),
            (.authCapabilityInstanceExhausted, "AUTH capability instance space exhausted"),
            (.noActiveSigner, "The active account has no registered signer"),
            (.invalidSignRequest("sentinel-request"), "Invalid sign request: sentinel-request"),
            (.signerUnavailable("sentinel-unavailable"), "Signer unavailable: sentinel-unavailable"),
            (.signerRejected("sentinel-rejection"), "Signer rejected the request: sentinel-rejection"),
            (.invalidSignerOutput("sentinel-output"), "Invalid signer output: sentinel-output"),
            (.receiptCorrelationIdExhausted, "Receipt correlation ID namespace exhausted"),
            (.storeOpenFailed("sentinel-open"), "Could not open store: sentinel-open"),
            (.storeResetFailed("sentinel-reset"), "Could not reset store: sentinel-reset"),
            (.storeStillOpen("/sentinel/store"), "Persistent store is still open: /sentinel/store"),
            (
                .engineStartFailed(component: "sentinel-component", reason: "sentinel-start"),
                "Engine could not start (sentinel-component): sentinel-start"
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
            (.noActiveAccount, "Group messages require an active account"),
            (.intentAlreadyConsumed, "This composed write intent was already published once"),
            (
                .relayInformationUnavailable(.http(reason: "sentinel-http")),
                "Relay information unavailable: NIP-11 HTTP request failed: sentinel-http"
            ),
            (
                .invalidCorrelationToken(got: "sentinel-token", reason: "sentinel-correlation"),
                #"Invalid correlation token "sentinel-token": sentinel-correlation"#
            ),
            (.invalidNip73Target(reason: "sentinel-target"), "Invalid NIP-73 target: sentinel-target"),
        ]

        for (error, expected) in cases {
            XCTAssertEqual(error.errorDescription, expected, "\(error)")
            XCTAssertEqual(error.localizedDescription, expected, "\(error)")
            XCTAssertFalse(error.localizedDescription.contains("NMP.NMPError error"))
        }
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
