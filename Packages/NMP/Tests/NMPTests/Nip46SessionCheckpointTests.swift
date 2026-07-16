import Foundation
import Security
import XCTest
@testable import NMP

/// #571: NMP-owned NIP-46 session checkpoint serialization, the Keychain
/// store, and the restore/import doors' no-checkpoint/refusal paths. The
/// live pair -> checkpoint -> restore -> resume-durable-write round trip is
/// exercised by the Rust-level falsifier
/// (`crates/nmp-engine/tests/nip46_restart.rs::checkpoint_restore_reattaches_durable_write_without_repairing`),
/// which this Swift wrapper composes verbatim -- there is no independent
/// Swift-owned NIP-46 transport to re-prove here (see that method's own
/// doc), and no real remote-signer counterpart is available in this
/// environment for a live end-to-end pairing test.
final class Nip46SessionCheckpointTests: XCTestCase {
    private func makeCheckpoint(
        origin: NMPNip46SessionOrigin = .bunker
    ) -> NMPNip46SessionCheckpoint {
        NMPNip46SessionCheckpoint(
            clientSecretKey: String(repeating: "a", count: 64),
            userPublicKey: String(repeating: "b", count: 64),
            remoteSignerPublicKey: String(repeating: "c", count: 64),
            relays: ["wss://relay.example", "wss://relay2.example"],
            origin: origin
        )
    }

    func testSerializeDeserializeRoundTripsEveryField() throws {
        for origin: NMPNip46SessionOrigin in [.clientInitiated, .bunker] {
            let checkpoint = makeCheckpoint(origin: origin)
            let data = try checkpoint.serialize()
            let restored = try NMPNip46SessionCheckpoint.deserialize(data)
            XCTAssertEqual(restored, checkpoint)
        }
    }

    func testDeserializeRejectsUnsupportedVersion() throws {
        let checkpoint = makeCheckpoint()
        var wire = try JSONSerialization.jsonObject(
            with: checkpoint.serialize()
        ) as! [String: Any]
        wire["version"] = 99
        let data = try JSONSerialization.data(withJSONObject: wire)

        XCTAssertThrowsError(try NMPNip46SessionCheckpoint.deserialize(data)) { error in
            XCTAssertEqual(
                error as? NMPNip46SessionCheckpoint.SerializationError,
                .unsupportedVersion(99)
            )
        }
    }

    func testDeserializeRejectsUnknownOrigin() throws {
        let checkpoint = makeCheckpoint()
        var wire = try JSONSerialization.jsonObject(
            with: checkpoint.serialize()
        ) as! [String: Any]
        wire["origin"] = "carrier-pigeon"
        let data = try JSONSerialization.data(withJSONObject: wire)

        XCTAssertThrowsError(try NMPNip46SessionCheckpoint.deserialize(data)) { error in
            XCTAssertEqual(
                error as? NMPNip46SessionCheckpoint.SerializationError,
                .invalidOrigin("carrier-pigeon")
            )
        }
    }

    func testDeserializeRejectsMalformedBytes() {
        XCTAssertThrowsError(
            try NMPNip46SessionCheckpoint.deserialize(Data("not json".utf8))
        ) { error in
            XCTAssertEqual(error as? NMPNip46SessionCheckpoint.SerializationError, .malformed)
        }
    }

    /// #571 secrecy falsifier: neither `description` nor `debugDescription`
    /// -- what `po`/`print`/string interpolation surface -- may ever contain
    /// the raw client secret.
    func testDescriptionAndDebugDescriptionRedactTheClientSecret() {
        let checkpoint = makeCheckpoint()

        XCTAssertFalse("\(checkpoint)".contains(checkpoint.clientSecretKey))
        XCTAssertFalse("\(checkpoint.debugDescription)".contains(checkpoint.clientSecretKey))
        XCTAssertTrue("\(checkpoint)".contains("[redacted]"))
    }

    func testRestoreFromStoreReturnsNilWithoutConnectingWhenNoCheckpointExists() throws {
        final class EmptyCheckpointStore: NMPNip46SessionCheckpointStore, @unchecked Sendable {
            func loadCheckpoint() throws -> NMPNip46SessionCheckpoint? { nil }
            func saveCheckpoint(_ checkpoint: NMPNip46SessionCheckpoint) throws {}
            func clear() throws {}
        }

        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let connection = try engine.restoreNip46Session(from: EmptyCheckpointStore())
        XCTAssertNil(connection)
    }

    /// A connection that has not reached `.ready` refuses `checkpoint()`
    /// with a typed error rather than persisting meaningless material.
    func testCheckpointBeforeReadyIsRefused() {
        let connection = NMPNip46Connection(observer: NIP46Observer(), closeAction: {})
        XCTAssertThrowsError(try connection.checkpoint())
    }
}
