// RestrictedReadProbe.swift
//
// One plain, ordinary, NMP-free Nostr client issuing ONE `REQ` on ONE fresh
// connection, and reporting exactly what the relay did about it: did it
// emit a real `["AUTH", <challenge>]` frame, did it refuse the
// subscription, and which events (if any) did it actually serve.
//
// This exists to establish the PRECONDITION of C15, and it must be
// NMP-free to do that job. "NMP received the event, therefore the relay
// demanded AUTH" is circular: a relay that never challenged anybody would
// make that assertion pass just as well. So a second, independent client
// -- this one -- proves the relay's demand first, and it proves it the
// only way that cannot be faked from the app side: by being refused.
//
// It is also the only place that can prove the negative half, that
// authenticating as the WRONG identity is not enough. strfry's
// `restrictReadToInvolvedPubkey` filters restricted-kind events per
// subscriber, so a client that authenticates as some other key gets a
// perfectly healthy subscription and zero rows. Without that measurement,
// "NMP authenticated" and "NMP authenticated as its own account" are
// indistinguishable.
//
// Every read is bounded (`WireConnection.receiveLine`), so this can report
// "the relay said nothing" but can never hang a scenario.

import Foundation

/// What one `REQ` on one fresh connection actually produced. Deliberately
/// raw: the challenge string verbatim, the relay's own refusal text
/// verbatim, and the ids it served. A scenario decides what these mean;
/// this type decides nothing.
public struct RestrictedReadProbe: Sendable {
    /// The verbatim challenge from an `["AUTH", <challenge>]` frame, if the
    /// relay sent one. `nil` means the relay never demanded authentication.
    public var challenge: String?
    /// The relay's own `CLOSED` message for this subscription, verbatim
    /// (e.g. `auth-required: requested filter requires authentication`).
    public var closedMessage: String?
    /// Whether an `AUTH` event was sent on this connection, and what the
    /// relay's `OK` for it said. `nil` when no handshake was attempted.
    public var authAccepted: Bool?
    public var authMessage: String?
    /// Event ids the relay actually delivered for this subscription.
    public var servedEventIDs: [String] = []
    /// Whether the relay reached end of stored events -- i.e. answered the
    /// question rather than refusing it.
    public var reachedEOSE = false
    /// Every frame observed, in arrival order. Printed as evidence when an
    /// assertion about the relay's behaviour fails.
    public var frames: [String] = []
}

public extension RelayHandle {
    /// Issues one `REQ` for `filter` on a FRESH connection and reports what
    /// the relay did.
    ///
    /// If the relay answers with an `AUTH` challenge and `authenticateAs`
    /// is non-nil, this signs a kind:22242 event for that exact challenge
    /// and this relay's URL, sends it, and re-issues the SAME filter on the
    /// SAME connection -- the five-step handshake that only works if one
    /// client owns the connection throughout (see `NIP42Client`, and the
    /// `nak --auth` limitation recorded in `docs/internals/canary.md`).
    ///
    /// With `authenticateAs` nil, the probe deliberately does NOT
    /// authenticate: that is the measurement of what an unauthenticated
    /// client can see.
    func probeRead(
        filter: [String: Any],
        authenticateAs keyPair: NostrKeyPair? = nil,
        timeout: TimeInterval = 5
    ) async throws -> RestrictedReadProbe {
        guard let wsURL = URL(string: url) else { throw RelayLabError.portAllocationFailed }
        let conn = WireConnection(url: wsURL)
        defer { conn.close() }

        var probe = RestrictedReadProbe()
        var subID = "canary-probe-\(Int.random(in: 0..<1_000_000))"
        try await conn.send(Self.reqFrame(subID: subID, filter: filter))

        var authenticated = false
        let deadline = Date().addingTimeInterval(timeout)

        while Date() < deadline {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { break }
            let line: String
            do {
                line = try await conn.receiveLine(timeout: remaining, what: "probe frame")
            } catch {
                // A bounded read that found nothing is a real answer here
                // ("the relay stopped talking"), not a scenario failure.
                break
            }
            probe.frames.append(line)

            guard let data = line.data(using: .utf8),
                  let arr = try? JSONSerialization.jsonObject(with: data) as? [Any],
                  let tag = arr.first as? String
            else { continue }

            switch tag {
            case "AUTH" where arr.count >= 2:
                probe.challenge = arr[1] as? String
                guard let keyPair, let challenge = probe.challenge, !authenticated else { continue }
                let authEvent = try NostrSigning.signAuthEvent(
                    keyPair: keyPair, relayURL: url, challenge: challenge
                )
                let authData = try JSONEncoder().encode(authEvent)
                try await conn.send("[\"AUTH\",\(String(data: authData, encoding: .utf8)!)]")
                authenticated = true
                // Re-issue the SAME filter under a fresh subscription id on
                // this now-authenticating connection. The relay already
                // CLOSED the first one.
                subID = "canary-probe-\(Int.random(in: 0..<1_000_000))"
                try await conn.send(Self.reqFrame(subID: subID, filter: filter))

            case "OK" where arr.count >= 3 && authenticated && probe.authAccepted == nil:
                probe.authAccepted = arr[2] as? Bool
                probe.authMessage = (arr.count >= 4 ? arr[3] as? String : nil) ?? ""

            case "CLOSED" where arr.count >= 3:
                probe.closedMessage = arr[2] as? String
                // Terminal only for the subscription currently outstanding.
                // strfry sends the AUTH challenge and THEN the CLOSED for
                // the refused subscription, so by the time this arrives the
                // handshake above has usually already opened a second
                // subscription -- returning on the first one's CLOSED would
                // report the pre-authentication refusal as the final answer
                // every time, and the authenticated probe would never
                // observe a single row.
                if (arr[1] as? String) == subID { return probe }

            case "EVENT" where arr.count >= 3:
                if let ev = arr[2] as? [String: Any], let id = ev["id"] as? String {
                    probe.servedEventIDs.append(id)
                }

            case "EOSE":
                probe.reachedEOSE = true
                return probe

            default:
                continue
            }
        }
        return probe
    }

    private static func reqFrame(subID: String, filter: [String: Any]) -> String {
        let filterData = (try? JSONSerialization.data(withJSONObject: filter)) ?? Data("{}".utf8)
        let filterStr = String(data: filterData, encoding: .utf8) ?? "{}"
        return "[\"REQ\",\"\(subID)\",\(filterStr)]"
    }
}
