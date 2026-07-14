import Foundation

struct MergeResult {
    let content: String
    let hasConflicts: Bool
}

protocol ThreeWayMerging {
    func merge(local: String, base: String, upstream: String) throws -> MergeResult
}

struct GitThreeWayMerger: ThreeWayMerging {
    func merge(local: String, base: String, upstream: String) throws -> MergeResult {
        if local == base { return MergeResult(content: upstream, hasConflicts: false) }
        if upstream == base || local == upstream { return MergeResult(content: local, hasConflicts: false) }

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("nmp-ui-merge-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        let localURL = directory.appendingPathComponent("local")
        let baseURL = directory.appendingPathComponent("base")
        let upstreamURL = directory.appendingPathComponent("upstream")
        try Data(local.utf8).write(to: localURL)
        try Data(base.utf8).write(to: baseURL)
        try Data(upstream.utf8).write(to: upstreamURL)

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = [
            "git", "merge-file", "-p", "--diff3",
            "-L", "local", "-L", "upstream base", "-L", "upstream",
            localURL.path, baseURL.path, upstreamURL.path,
        ]
        let output = Pipe()
        let errors = Pipe()
        process.standardOutput = output
        process.standardError = errors
        try process.run()
        process.waitUntilExit()

        let mergedData = output.fileHandleForReading.readDataToEndOfFile()
        let errorData = errors.fileHandleForReading.readDataToEndOfFile()
        guard process.terminationStatus == 0 || process.terminationStatus == 1 else {
            let message = String(data: errorData, encoding: .utf8) ?? "git merge-file exited \(process.terminationStatus)"
            throw NMPUIError.mergeFailed(message.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        guard let merged = String(data: mergedData, encoding: .utf8) else {
            throw NMPUIError.mergeFailed("git merge-file returned non-UTF-8 output")
        }
        return MergeResult(content: merged, hasConflicts: process.terminationStatus == 1)
    }
}
