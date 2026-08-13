import Foundation
import XCTest
@testable import NMP

final class SessionTests: XCTestCase {
    private let privateKey = String(repeating: "0", count: 63) + "1"
    private let privatePublicKey =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    private let publicKeyOnly =
        "c6047f9441ed7d6d3045406e95c07cd85a2b081b817960f2a6f9d5b80f4fcdcf"

    func testWholeSessionRoundTripsSignerBackedAndPublicKeyOnlyAccounts() throws {
        let first = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 4))
        let signerBacked = try first.session.add(privateKey: testPrivateKey(privateKey))
        let readOnly = try first.session.add(
            publicKey: testPublicKey(publicKeyOnly),
            makeCurrent: true
        )

        XCTAssertEqual(signerBacked.publicKey, try testPublicKey(privatePublicKey))
        XCTAssertEqual(signerBacked.providerKind, .localKey)
        XCTAssertEqual(signerBacked.signingAvailability, .available)
        XCTAssertNil(readOnly.providerKind)
        XCTAssertEqual(readOnly.signingAvailability, .unsupported)
        XCTAssertEqual(try first.session.current?.publicKey, try testPublicKey(publicKeyOnly))

        let payload = try first.session.export()
        let firstKeys = try first.session.accounts.map(\.publicKey)
        first.shutdown()

        let restored = try NMPEngine(
            config: NMPConfig(maxAuthCapabilities: 4),
            sessionPayload: NMPSessionPayload(bytes: payload.bytes)
        )
        defer { restored.shutdown() }

        let expectedPrivatePublicKey = try testPublicKey(privatePublicKey)
        let expectedPublicKeyOnly = try testPublicKey(publicKeyOnly)
        XCTAssertEqual(try restored.session.accounts.map(\.publicKey), firstKeys)
        XCTAssertEqual(try restored.session.current?.publicKey, expectedPublicKeyOnly)
        XCTAssertEqual(
            try restored.session.accounts.first { $0.publicKey == expectedPrivatePublicKey }?
                .providerKind,
            .localKey
        )
        XCTAssertNil(
            try restored.session.accounts.first { $0.publicKey == expectedPublicKeyOnly }?
                .providerKind
        )
    }

    func testRemoveCurrentAccountClearsSelectionAndAccountIdentityIsItsPublicKey() throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 4))
        defer { engine.shutdown() }

        let original = try engine.session.add(
            privateKey: testPrivateKey(privateKey),
            makeCurrent: true
        )
        XCTAssertTrue(try engine.session.remove(original))
        XCTAssertNil(try engine.session.current)

        let replacement = try engine.session.add(
            privateKey: testPrivateKey(privateKey),
            makeCurrent: true
        )
        XCTAssertTrue(try engine.session.remove(original))
        XCTAssertNil(try engine.session.current)
        _ = try engine.session.add(privateKey: testPrivateKey(privateKey), makeCurrent: true)
        XCTAssertEqual(try engine.session.current?.publicKey, replacement.publicKey)
    }

    func testClearRemovesAccountsAndCurrentSelection() throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 4))
        defer { engine.shutdown() }

        _ = try engine.session.add(privateKey: testPrivateKey(privateKey))
        _ = try engine.session.add(
            publicKey: testPublicKey(publicKeyOnly),
            makeCurrent: true
        )
        try engine.session.clear()

        XCTAssertTrue(try engine.session.accounts.isEmpty)
        XCTAssertNil(try engine.session.current)
    }

    func testOpaquePayloadDescriptionsAreRedacted() {
        let payload = NMPSessionPayload(bytes: Data([0xde, 0xad, 0xbe, 0xef]))

        XCTAssertEqual(payload.description, "NMPSessionPayload(<redacted>)")
        XCTAssertEqual(payload.debugDescription, "NMPSessionPayload(<redacted>)")
        XCTAssertFalse(String(describing: payload).contains("deadbeef"))
        XCTAssertFalse(String(reflecting: payload).contains("deadbeef"))
    }

    func testGeneratedPrivateKeyIsRedactedAndCanBackAnAccount() throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 4))
        defer { engine.shutdown() }

        let privateKey = NMPPrivateKey.generate()
        XCTAssertEqual(privateKey.description, "NMPPrivateKey(<redacted>)")
        XCTAssertEqual(privateKey.debugDescription, "NMPPrivateKey(<redacted>)")

        let account = try engine.session.add(privateKey: privateKey, makeCurrent: true)
        XCTAssertEqual(account.providerKind, .localKey)
        XCTAssertEqual(account.signingAvailability, .available)
        XCTAssertEqual(try engine.session.current?.publicKey, account.publicKey)
    }

    func testCorruptPayloadRefusesEngineConstruction() {
        let payload = NMPSessionPayload(bytes: Data([0xde, 0xad, 0xbe, 0xef]))
        XCTAssertThrowsError(try NMPEngine(config: NMPConfig(), sessionPayload: payload))
    }
}
