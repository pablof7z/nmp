import Foundation

struct LockState: Codable, Equatable {
    var schemaVersion = 1
    var registryVersion: String
    var components: [String: LockedComponent]

    static func empty(registryVersion: String) -> LockState {
        LockState(registryVersion: registryVersion, components: [:])
    }
}

struct LockedComponent: Codable, Equatable {
    let version: String
    let dependencies: [String]
    let files: [String: LockedFile]
}

struct LockedFile: Codable, Equatable {
    let upstreamHash: String
    let upstreamBase: String
}

struct ConflictState: Codable, Equatable {
    var schemaVersion = 1
    var components: [String: ConflictedComponent]
}

struct ConflictedComponent: Codable, Equatable {
    let fromVersion: String
    let toVersion: String
    let paths: [String]
}

extension ProjectFileSystem {
    func transactionWrites(_ writes: [(String, String)], lock: LockState) throws -> [String: Data] {
        var result = Dictionary(uniqueKeysWithValues: writes.map { ($0.0, Data($0.1.utf8)) })
        result[Self.lockPath] = try encodedLock(lock)
        return result
    }

    func transactionWrites(_ writes: [String: String], lock: LockState) throws -> [String: Data] {
        var result = writes.mapValues { Data($0.utf8) }
        result[Self.lockPath] = try encodedLock(lock)
        return result
    }

    func loadLock(registryVersion: String) throws -> LockState {
        guard let data = try readDataIfPresent(Self.lockPath) else {
            return .empty(registryVersion: registryVersion)
        }
        return try JSONDecoder().decode(LockState.self, from: data)
    }

    func saveLock(_ lock: LockState) throws {
        try write(encodedLock(lock), to: Self.lockPath)
    }

    private func encodedLock(_ lock: LockState) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        var data = try encoder.encode(lock)
        data.append(0x0a)
        return data
    }

    func loadConflicts() throws -> ConflictState {
        guard let data = try readDataIfPresent(Self.conflictPath) else {
            return ConflictState(components: [:])
        }
        return try JSONDecoder().decode(ConflictState.self, from: data)
    }

    func saveConflicts(_ conflicts: ConflictState) throws {
        if conflicts.components.isEmpty {
            try remove(Self.conflictPath)
            return
        }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        var data = try encoder.encode(conflicts)
        data.append(0x0a)
        try write(data, to: Self.conflictPath)
    }
}
