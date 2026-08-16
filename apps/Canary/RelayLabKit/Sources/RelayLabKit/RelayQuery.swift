// RelayQuery.swift
//
// Minimal REQ/EVENT/EOSE client used only to confirm what `seed` proved was
// written round-trips back over the wire -- e.g. the actual persistence
// check after a kill+restart.

import Foundation

public extension RelayHandle {
    /// Opens a fresh connection, sends `["REQ", subId, {"ids": [id]}]`, and
    /// returns the matching event if the relay sends it back before EOSE
    /// or `timeout` elapses. Returns `nil` (not a hang) if the relay never
    /// answers.
    func queryById(_ id: String, timeout: TimeInterval = 5) async throws -> [String: Any]? {
        guard let wsURL = URL(string: url) else { return nil }
        let conn = WireConnection(url: wsURL)
        defer { conn.close() }

        let subId = "canary-\(Int.random(in: 0..<1_000_000))"
        let filter = ["ids": [id]] as [String: Any]
        let filterData = try JSONSerialization.data(withJSONObject: filter)
        let filterStr = String(data: filterData, encoding: .utf8)!
        try await conn.send("[\"REQ\",\"\(subId)\",\(filterStr)]")

        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { break }
            let line = try await conn.receiveLine(timeout: remaining, what: "REQ result for \(id)")
            guard let data = line.data(using: .utf8),
                  let arr = try? JSONSerialization.jsonObject(with: data) as? [Any],
                  let tag = arr.first as? String
            else { continue }
            if tag == "EVENT", arr.count >= 3, let ev = arr[2] as? [String: Any],
               ev["id"] as? String == id {
                return ev
            }
            if tag == "EOSE" {
                return nil
            }
        }
        return nil
    }
}
