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
    _ stream: NMPGroupWriteStatus,
    seconds: UInt64,
    predicate: @escaping @Sendable ([WriteStatus]) -> Bool
) async throws -> [WriteStatus] {
    try await withTimeout(seconds: seconds) {
        var statuses: [WriteStatus] = []
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

func acked(_ statuses: [WriteStatus], relay: String) -> Bool {
    statuses.contains {
        if case .acked(let candidate) = $0 { return candidate == relay }
        return false
    }
}

func rejected(_ statuses: [WriteStatus], relay: String) -> Bool {
    statuses.contains {
        if case .rejected(let candidate, _) = $0 { return candidate == relay }
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
