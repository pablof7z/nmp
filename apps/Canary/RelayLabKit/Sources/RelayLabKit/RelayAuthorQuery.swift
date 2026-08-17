// RelayAuthorQuery.swift
//
// The counting companion to `queryById`. `queryById` answers "is this exact
// event here", which cannot see the failure C10 is looking for: a write
// that goes out twice as two DIFFERENT events (a re-signed intent with a
// fresh `created_at` is a different id, so a relay's own id-dedup hides
// nothing and reveals nothing). Asking the relay for everything one author
// wrote is what distinguishes "delivered once" from "delivered twice".
//
// Kept in its own file rather than added to `RelayQuery.swift` because the
// Canary's scenario files are being written concurrently and an additive
// file is the change that cannot conflict.

import Foundation

public extension RelayHandle {
    /// Opens a fresh connection, sends
    /// `["REQ", subId, {"authors": [pubkey], "kinds": kinds}]`, and returns
    /// every event id the relay serves back before EOSE or `timeout`.
    /// Returns `[]` (not a hang) if the relay answers with nothing.
    func queryIDsByAuthor(
        _ pubkeyHex: String,
        kinds: [Int],
        timeout: TimeInterval = 5
    ) async throws -> [String] {
        guard let wsURL = URL(string: url) else { return [] }
        let conn = WireConnection(url: wsURL)
        defer { conn.close() }

        let subId = "canary-author-\(Int.random(in: 0..<1_000_000))"
        let filter = ["authors": [pubkeyHex], "kinds": kinds] as [String: Any]
        let filterData = try JSONSerialization.data(withJSONObject: filter)
        let filterStr = String(data: filterData, encoding: .utf8)!
        try await conn.send("[\"REQ\",\"\(subId)\",\(filterStr)]")

        var ids: [String] = []
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { break }
            let line = try await conn.receiveLine(
                timeout: remaining, what: "REQ result for author \(pubkeyHex)"
            )
            guard let data = line.data(using: .utf8),
                  let arr = try? JSONSerialization.jsonObject(with: data) as? [Any],
                  let tag = arr.first as? String
            else { continue }
            if tag == "EVENT", arr.count >= 3, let ev = arr[2] as? [String: Any],
               let id = ev["id"] as? String {
                ids.append(id)
            }
            if tag == "EOSE" { return ids }
        }
        return ids
    }
}
