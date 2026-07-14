import Foundation

struct ProjectFileSystem {
    static let lockPath = ".nmp-ui-lock.json"
    static let conflictPath = ".nmp-ui-conflicts.json"

    let root: URL
    private let manager = FileManager.default

    init(root: URL) {
        self.root = root.standardizedFileURL
    }

    func resolve(_ relativePath: String) throws -> URL {
        let pieces = relativePath.split(separator: "/", omittingEmptySubsequences: false)
        guard !relativePath.isEmpty,
              !relativePath.hasPrefix("/"),
              !relativePath.contains("\\"),
              pieces.allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else {
            throw NMPUIError.unsafePath(relativePath)
        }
        let candidate = root.appendingPathComponent(relativePath).standardizedFileURL
        let rootPath = root.path.hasSuffix("/") ? root.path : root.path + "/"
        guard candidate.path.hasPrefix(rootPath) else { throw NMPUIError.unsafePath(relativePath) }
        return candidate
    }

    func exists(_ path: String) throws -> Bool {
        manager.fileExists(atPath: try resolve(path).path)
    }

    func read(_ path: String) throws -> String {
        try String(contentsOf: resolve(path), encoding: .utf8)
    }

    func readDataIfPresent(_ path: String) throws -> Data? {
        let url = try resolve(path)
        guard manager.fileExists(atPath: url.path) else { return nil }
        return try Data(contentsOf: url)
    }

    func write(_ content: String, to path: String) throws {
        let url = try resolve(path)
        try manager.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data(content.utf8).write(to: url, options: .atomic)
    }

    func write(_ data: Data, to path: String) throws {
        let url = try resolve(path)
        try manager.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try data.write(to: url, options: .atomic)
    }

    func remove(_ path: String) throws {
        let url = try resolve(path)
        if manager.fileExists(atPath: url.path) { try manager.removeItem(at: url) }
    }
}
