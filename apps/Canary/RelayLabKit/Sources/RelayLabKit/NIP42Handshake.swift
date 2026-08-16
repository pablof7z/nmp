// NIP42Handshake.swift
//
// The NIP-42 client handshake implemented directly by the lab controller
// (challenge -> sign -> resubmit), rather than shelling out to a third-party
// CLI. This exists specifically because the earlier Python-prototype
// investigation could NOT get a live "write accepted after AUTH" result
// via `nak event --auth` (a client-tool quirk, not a strfry defect --
// confirmed from strfry's own source, see the investigation report). This
// is the fix attempt: drive the handshake ourselves, on ONE connection,
// so there is no ambiguity about which connection's AUTH session a retry
// belongs to.

import Foundation

public enum NIP42Result {
    case acceptedWithoutAuth
    case acceptedAfterAuth(challenge: String)
    case deniedAfterAuth(challenge: String, message: String)
    case deniedNoChallengeOffered(message: String)
}

public enum NIP42Error: Error, CustomStringConvertible {
    case noChallengeReceived
    case unexpectedFrame(String)

    public var description: String {
        switch self {
        case .noChallengeReceived: return "relay refused but never sent an AUTH challenge"
        case .unexpectedFrame(let f): return "unexpected frame: \(f)"
        }
    }
}

public enum NIP42Client {
    /// Publishes `event` on a SINGLE fresh connection, and if the relay
    /// responds with `auth-required` + an `AUTH` challenge, signs a
    /// kind:22242 auth event for that exact challenge and relay URL,
    /// sends it, then resubmits the ORIGINAL event on the SAME
    /// connection. Every read is bounded; this never hangs waiting for a
    /// frame that isn't coming.
    public static func publishWithAuth(
        event: NostrEvent,
        relayURL: String,
        keyPair: NostrKeyPair,
        perFrameTimeout: TimeInterval = 5
    ) async throws -> NIP42Result {
        guard let wsURL = URL(string: relayURL) else { throw NIP42Error.unexpectedFrame(relayURL) }
        let conn = WireConnection(url: wsURL)
        defer { conn.close() }

        try await conn.send(event.eventFrame())

        var challenge: String?
        var deniedMessage: String?

        // The relay may push the AUTH challenge and the OK in either
        // order (strfry's own source sends AUTH then OK, but nothing
        // guarantees a client sees them in that order at the transport
        // level) -- read up to 2 frames, bounded, and react to whichever
        // arrives.
        for _ in 0..<2 {
            let line = try await conn.receiveLine(timeout: perFrameTimeout, what: "initial OK/AUTH")
            switch RelayFrame.parse(line) {
            case .ok(let id, let accepted, let message) where id == event.id:
                if accepted { return .acceptedWithoutAuth }
                deniedMessage = message
            case .authChallenge(let c):
                challenge = c
            default:
                continue
            }
            if challenge != nil, deniedMessage != nil { break }
        }

        guard let challenge else {
            throw NIP42Error.noChallengeReceived
        }
        guard let deniedMessage else {
            // Got a challenge but no denial yet (unusual ordering) --
            // treat as "challenge offered, proceed to authenticate".
            return try await authenticateAndRetry(
                conn: conn, event: event, relayURL: relayURL, challenge: challenge,
                keyPair: keyPair, perFrameTimeout: perFrameTimeout
            )
        }
        return try await authenticateAndRetry(
            conn: conn, event: event, relayURL: relayURL, challenge: challenge,
            keyPair: keyPair, perFrameTimeout: perFrameTimeout, deniedMessage: deniedMessage
        )
    }

    private static func authenticateAndRetry(
        conn: WireConnection, event: NostrEvent, relayURL: String, challenge: String,
        keyPair: NostrKeyPair, perFrameTimeout: TimeInterval, deniedMessage: String = ""
    ) async throws -> NIP42Result {
        let authEvent = try NostrSigning.signAuthEvent(
            keyPair: keyPair, relayURL: relayURL, challenge: challenge
        )
        let authData = try JSONEncoder().encode(authEvent)
        let authObj = String(data: authData, encoding: .utf8)!
        try await conn.send("[\"AUTH\",\(authObj)]")

        // The relay may or may not explicitly ack the AUTH event itself
        // (NIP-42 doesn't mandate it); either way, resubmit the ORIGINAL
        // event on this now-authenticated connection and wait for ITS
        // final OK, ignoring any unrelated frames (e.g. an OK for the
        // auth event) in between.
        try await conn.send(event.eventFrame())

        let deadline = Date().addingTimeInterval(perFrameTimeout * 3)
        while Date() < deadline {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { break }
            let line = try await conn.receiveLine(timeout: remaining, what: "OK after AUTH")
            if case .ok(let id, let accepted, let message) = RelayFrame.parse(line), id == event.id {
                return accepted
                    ? .acceptedAfterAuth(challenge: challenge)
                    : .deniedAfterAuth(challenge: challenge, message: message)
            }
        }
        return .deniedAfterAuth(challenge: challenge, message: "\(deniedMessage) (no final OK observed within timeout)")
    }
}
