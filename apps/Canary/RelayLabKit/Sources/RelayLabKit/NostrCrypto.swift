// NostrCrypto.swift
//
// Minimal, correct-enough (NOT a general-purpose library) NIP-01 event
// construction and BIP-340 Schnorr signing for the Canary relay lab
// CONTROLLER. This is client-side code the lab uses to seed relays and to
// drive the NIP-42 AUTH handshake -- it deliberately does not touch any NMP
// crate; the whole point of the lab is that it is a plain, ordinary Nostr
// client talking to a real, external relay process.

import Foundation
import CryptoKit
import P256K

public struct NostrKeyPair {
    public let privateKey: P256K.Schnorr.PrivateKey
    public let pubkeyHex: String

    public init() throws {
        privateKey = try P256K.Schnorr.PrivateKey()
        pubkeyHex = Self.hex(privateKey.xonly.bytes)
    }

    static func hex(_ bytes: [UInt8]) -> String {
        bytes.map { String(format: "%02x", $0) }.joined()
    }
}

public struct NostrEvent: Codable {
    public var id: String
    public var pubkey: String
    public var created_at: Int64
    public var kind: Int
    public var tags: [[String]]
    public var content: String
    public var sig: String

    /// The `["EVENT", <event>]` client-message wire frame.
    public func eventFrame() throws -> String {
        let data = try JSONEncoder().encode(self)
        let obj = String(data: data, encoding: .utf8)!
        return "[\"EVENT\",\(obj)]"
    }
}

public enum NostrSigning {
    /// NIP-01 canonical serialization for the id hash:
    /// `[0, pubkey, created_at, kind, tags, content]`, compact, no whitespace.
    public static func serializeForId(
        pubkey: String, createdAt: Int64, kind: Int, tags: [[String]], content: String
    ) -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.withoutEscapingSlashes]
        let tagsData = (try? encoder.encode(tags)) ?? Data("[]".utf8)
        let contentData = (try? encoder.encode(content)) ?? Data("\"\"".utf8)
        let tagsStr = String(data: tagsData, encoding: .utf8) ?? "[]"
        let contentStr = String(data: contentData, encoding: .utf8) ?? "\"\""
        let json = "[0,\"\(pubkey)\",\(createdAt),\(kind),\(tagsStr),\(contentStr)]"
        return Data(json.utf8)
    }

    public static func sign(
        keyPair: NostrKeyPair,
        kind: Int,
        tags: [[String]] = [],
        content: String,
        createdAt: Int64 = Int64(Date().timeIntervalSince1970)
    ) throws -> NostrEvent {
        let serialized = serializeForId(
            pubkey: keyPair.pubkeyHex, createdAt: createdAt, kind: kind, tags: tags, content: content
        )
        let digest = SHA256.hash(data: serialized)
        let idHex = digest.map { String(format: "%02x", $0) }.joined()
        let sig = try keyPair.privateKey.signature(for: digest)
        let sigHex = sig.dataRepresentation.map { String(format: "%02x", $0) }.joined()
        return NostrEvent(
            id: idHex, pubkey: keyPair.pubkeyHex, created_at: createdAt, kind: kind,
            tags: tags, content: content, sig: sigHex
        )
    }

    /// A NIP-42 kind:22242 auth event: `tags = [["relay", url], ["challenge", challenge]]`.
    public static func signAuthEvent(
        keyPair: NostrKeyPair, relayURL: String, challenge: String,
        createdAt: Int64 = Int64(Date().timeIntervalSince1970)
    ) throws -> NostrEvent {
        try sign(
            keyPair: keyPair, kind: 22242,
            tags: [["relay", relayURL], ["challenge", challenge]],
            content: "", createdAt: createdAt
        )
    }
}
