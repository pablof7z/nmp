import Foundation
import NMP

enum ProbeError: Error, CustomStringConvertible, Sendable {
    case message(String)
    case timeout(UInt64)

    var description: String {
        switch self {
        case .message(let value): return value
        case .timeout(let seconds): return "operation timed out after \(seconds)s"
        }
    }
}

func require(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    guard condition() else { throw ProbeError.message(message) }
}

/// Decode this command-line tool's existing hex fixtures at its human/file
/// input boundary. NMP itself receives only validated decoded key bytes.
func decodedHex(_ value: String) throws -> Data {
    guard value.count.isMultiple(of: 2) else {
        throw ProbeError.message("hex key has odd length")
    }
    var bytes: [UInt8] = []
    bytes.reserveCapacity(value.count / 2)
    var index = value.startIndex
    while index < value.endIndex {
        let next = value.index(index, offsetBy: 2)
        guard let byte = UInt8(value[index..<next], radix: 16) else {
            throw ProbeError.message("key is not valid hex")
        }
        bytes.append(byte)
        index = next
    }
    return Data(bytes)
}

func hex(_ data: Data) -> String {
    data.map { String(format: "%02x", $0) }.joined()
}

func withTimeout<T: Sendable>(
    seconds: UInt64,
    operation: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await operation() }
        group.addTask {
            try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
            throw ProbeError.timeout(seconds)
        }
        defer { group.cancelAll() }
        guard let result = try await group.next() else {
            throw ProbeError.message("timed operation produced no result")
        }
        return result
    }
}

func pollUntil(
    seconds: UInt64,
    predicate: @escaping @Sendable () async throws -> Bool
) async throws {
    try await withTimeout(seconds: seconds) {
        while !(try await predicate()) {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
    }
}

func waitForFile(_ path: String, seconds: UInt64) async throws {
    try await pollUntil(seconds: seconds) {
        FileManager.default.fileExists(atPath: path)
    }
}

func waitForRows(
    _ query: NMPQuery,
    seconds: UInt64,
    predicate: @escaping @Sendable (RowBatch) -> Bool
) async throws -> RowBatch {
    try await withTimeout(seconds: seconds) {
        for try await batch in query {
            if predicate(batch) { return batch }
        }
        throw ProbeError.message("row stream ended before its predicate")
    }
}

func waitForStatuses(
    _ stream: ReceiptStatus,
    seconds: UInt64,
    predicate: @escaping @Sendable ([WriteFact]) -> Bool
) async throws -> [WriteFact] {
    try await withTimeout(seconds: seconds) {
        var statuses: [WriteFact] = []
        for try await status in stream {
            statuses.append(status)
            if predicate(statuses) { return statuses }
        }
        throw ProbeError.message("write stream ended before its predicate: \(statuses)")
    }
}

func waitForDiagnostics(
    _ diagnostics: NMPDiagnostics,
    seconds: UInt64,
    predicate: @escaping @Sendable (DiagnosticsSnapshot) -> Bool
) async throws -> DiagnosticsSnapshot {
    try await withTimeout(seconds: seconds) {
        for try await snapshot in diagnostics {
            if predicate(snapshot) { return snapshot }
        }
        throw ProbeError.message("diagnostics ended before its predicate")
    }
}

func rows(_ batch: RowBatch, kind: UInt16) -> [Row] {
    batch.rows.filter { $0.kind == kind }
}

func tagValue(_ row: Row, _ name: String) -> String? {
    for tag in row.tags where tag.first == name {
        if tag.count > 1 { return tag[1] }
    }
    return nil
}

func hasContent(_ batch: RowBatch, _ content: String, sourceCount: Int) -> Bool {
    batch.rows.contains { $0.content == content && $0.sources.count == sourceCount }
}

/// The relay acked this event. Per-relay, not per-write: a four-relay
/// publish has four independent fates and this asks about exactly one.
func acked(_ statuses: [WriteFact], relay: String) -> Bool {
    statuses.contains {
        if case .relay(let candidate, .published) = $0 { return candidate == relay }
        return false
    }
}

/// The relay authenticated the identity and refused THIS EVENT. Deliberately
/// not satisfied by `.authFailed`, which is a different situation with a
/// different repair.
func rejected(_ statuses: [WriteFact], relay: String) -> Bool {
    statuses.contains {
        if case .relay(let candidate, .rejected) = $0 { return candidate == relay }
        return false
    }
}

func sourceEvidence(_ batch: RowBatch) -> [SourceEvidence] {
    batch.evidence.flatMap(\.sources)
}

func signalReady(_ path: String?) throws {
    guard let path else { return }
    try Data("ready\n".utf8).write(to: URL(fileURLWithPath: path), options: .atomic)
}
