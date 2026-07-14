import Foundation

#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

struct ProjectFileSystem {
    static let lockPath = ".nmp-ui-lock.json"
    static let conflictPath = ".nmp-ui-conflicts.json"

    enum MutationPoint: Equatable {
        case write(String)
        case remove(String)
    }

    typealias MutationHook = (MutationPoint) throws -> Void

    private final class RootDescriptor {
        let value: Int32

        init(path: String) throws {
            value = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
            guard value >= 0 else { throw Self.posixError() }
        }

        deinit { close(value) }

        private static func posixError() -> Error {
            POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
    }

    private struct Snapshot {
        let data: Data?
    }

    let root: URL
    private let descriptor: RootDescriptor
    private let mutationHook: MutationHook?

    init(root: URL, mutationHook: MutationHook? = nil) throws {
        let canonicalRoot = root.standardizedFileURL.resolvingSymlinksInPath()
        self.root = canonicalRoot
        self.descriptor = try RootDescriptor(path: canonicalRoot.path)
        self.mutationHook = mutationHook
    }

    func resolve(_ relativePath: String) throws -> URL {
        let pieces = try pathPieces(relativePath)
        return pieces.reduce(root) { $0.appendingPathComponent($1) }.standardizedFileURL
    }

    func exists(_ path: String) throws -> Bool {
        let target: (descriptor: Int32, name: String, createdDirectories: Set<String>)
        do {
            target = try openParentDirectory(for: path, createMissing: false)
        } catch let error as POSIXError where error.code == .ENOENT {
            return false
        }
        defer { close(target.descriptor) }
        return try itemExists(parent: target.descriptor, name: target.name, path: path)
    }

    func read(_ path: String) throws -> String {
        let data = try readData(path)
        guard let value = String(data: data, encoding: .utf8) else {
            throw CocoaError(.fileReadInapplicableStringEncoding)
        }
        return value
    }

    func readDataIfPresent(_ path: String) throws -> Data? {
        let target: (descriptor: Int32, name: String, createdDirectories: Set<String>)
        do {
            target = try openParentDirectory(for: path, createMissing: false)
        } catch let error as POSIXError where error.code == .ENOENT {
            return nil
        }
        defer { close(target.descriptor) }
        guard try itemExists(parent: target.descriptor, name: target.name, path: path) else { return nil }
        return try readData(parent: target.descriptor, name: target.name, path: path)
    }

    func write(_ content: String, to path: String) throws {
        _ = try writeRaw(Data(content.utf8), to: path, invokeHook: true)
    }

    func write(_ data: Data, to path: String) throws {
        _ = try writeRaw(data, to: path, invokeHook: true)
    }

    func remove(_ path: String) throws {
        try removeRaw(path, invokeHook: true)
    }

    func commit(writes: [String: Data], removals: Set<String>) throws {
        let targets = Set(writes.keys).union(removals)
        var snapshots: [String: Snapshot] = [:]
        for path in targets.sorted() {
            snapshots[path] = Snapshot(data: try readDataIfPresent(path))
        }

        var createdDirectories = Set<String>()
        var mutatedPaths: [String] = []
        do {
            for path in removals.sorted() {
                try removeRaw(path, invokeHook: true)
                mutatedPaths.append(path)
            }
            let ordinaryWrites = writes.keys.filter { $0 != Self.lockPath }.sorted()
            for path in ordinaryWrites {
                createdDirectories.formUnion(
                    try writeRaw(writes[path]!, to: path, invokeHook: true)
                )
                mutatedPaths.append(path)
            }
            if let lock = writes[Self.lockPath] {
                createdDirectories.formUnion(
                    try writeRaw(lock, to: Self.lockPath, invokeHook: true)
                )
                mutatedPaths.append(Self.lockPath)
            }
        } catch {
            let originalError = error
            do {
                for path in mutatedPaths.reversed() {
                    guard let snapshot = snapshots[path] else { continue }
                    if let data = snapshot.data {
                        _ = try writeRaw(data, to: path, invokeHook: false)
                    } else {
                        try removeRaw(path, invokeHook: false)
                    }
                }
                for directory in createdDirectories.sorted(by: deeperPathFirst) {
                    try removeDirectoryIfEmpty(directory)
                }
            } catch {
                throw NMPUIError.transactionFailed(
                    "mutation failed with \(originalError); rollback failed with \(error)"
                )
            }
            throw originalError
        }
    }

    private func readData(_ path: String) throws -> Data {
        let target = try openParentDirectory(for: path, createMissing: false)
        defer { close(target.descriptor) }
        return try readData(parent: target.descriptor, name: target.name, path: path)
    }

    private func readData(parent: Int32, name: String, path: String) throws -> Data {
        let fileDescriptor = name.withCString {
            openat(parent, $0, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
        }
        guard fileDescriptor >= 0 else {
            if errno == ELOOP { throw NMPUIError.unsafePath(path) }
            throw posixError()
        }
        let handle = FileHandle(fileDescriptor: fileDescriptor, closeOnDealloc: true)
        return try handle.readToEnd() ?? Data()
    }

    private func writeRaw(_ data: Data, to path: String, invokeHook: Bool) throws -> Set<String> {
        if invokeHook { try mutationHook?(.write(path)) }
        let target = try openParentDirectory(for: path, createMissing: true)
        defer { close(target.descriptor) }

        var metadata = stat()
        let status = target.name.withCString {
            fstatat(target.descriptor, $0, &metadata, AT_SYMLINK_NOFOLLOW)
        }
        if status == 0, metadata.st_mode & S_IFMT == S_IFLNK {
            throw NMPUIError.unsafePath(path)
        }
        if status != 0, errno != ENOENT { throw posixError() }

        let temporaryName = ".nmp-ui-tmp-\(UUID().uuidString)"
        let temporaryDescriptor = temporaryName.withCString {
            openat(
                target.descriptor,
                $0,
                O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
                mode_t(S_IRUSR | S_IWUSR | S_IRGRP | S_IROTH)
            )
        }
        guard temporaryDescriptor >= 0 else { throw posixError() }
        defer { close(temporaryDescriptor) }

        do {
            try data.withUnsafeBytes { bytes in
                guard var base = bytes.baseAddress else { return }
                var remaining = bytes.count
                while remaining > 0 {
                    let count = DarwinOrGlibcWrite(temporaryDescriptor, base, remaining)
                    guard count >= 0 else { throw posixError() }
                    remaining -= count
                    base = base.advanced(by: count)
                }
            }
            guard fsync(temporaryDescriptor) == 0 else { throw posixError() }
            let renamed = temporaryName.withCString { temporaryPointer in
                target.name.withCString { targetPointer in
                    renameat(target.descriptor, temporaryPointer, target.descriptor, targetPointer)
                }
            }
            guard renamed == 0 else { throw posixError() }
        } catch {
            temporaryName.withCString { _ = unlinkat(target.descriptor, $0, 0) }
            throw error
        }
        return target.createdDirectories
    }

    private func removeRaw(_ path: String, invokeHook: Bool) throws {
        if invokeHook { try mutationHook?(.remove(path)) }
        let target: (descriptor: Int32, name: String, createdDirectories: Set<String>)
        do {
            target = try openParentDirectory(for: path, createMissing: false)
        } catch let error as POSIXError where error.code == .ENOENT {
            return
        }
        defer { close(target.descriptor) }

        var metadata = stat()
        let status = target.name.withCString {
            fstatat(target.descriptor, $0, &metadata, AT_SYMLINK_NOFOLLOW)
        }
        if status != 0 {
            if errno == ENOENT { return }
            throw posixError()
        }
        guard metadata.st_mode & S_IFMT != S_IFLNK else { throw NMPUIError.unsafePath(path) }
        let result = target.name.withCString { unlinkat(target.descriptor, $0, 0) }
        guard result == 0 else { throw posixError() }
    }

    private func removeDirectoryIfEmpty(_ path: String) throws {
        let target: (descriptor: Int32, name: String, createdDirectories: Set<String>)
        do {
            target = try openParentDirectory(for: path, createMissing: false)
        } catch let error as POSIXError where error.code == .ENOENT {
            return
        }
        defer { close(target.descriptor) }
        var metadata = stat()
        let status = target.name.withCString {
            fstatat(target.descriptor, $0, &metadata, AT_SYMLINK_NOFOLLOW)
        }
        if status != 0 {
            if errno == ENOENT { return }
            throw posixError()
        }
        guard metadata.st_mode & S_IFMT != S_IFLNK else { throw NMPUIError.unsafePath(path) }
        let result = target.name.withCString { unlinkat(target.descriptor, $0, AT_REMOVEDIR) }
        if result != 0, errno != ENOTEMPTY, errno != EEXIST, errno != ENOENT { throw posixError() }
    }

    private func itemExists(parent: Int32, name: String, path: String) throws -> Bool {
        var metadata = stat()
        let status = name.withCString {
            fstatat(parent, $0, &metadata, AT_SYMLINK_NOFOLLOW)
        }
        if status != 0 {
            if errno == ENOENT { return false }
            throw posixError()
        }
        guard metadata.st_mode & S_IFMT != S_IFLNK else { throw NMPUIError.unsafePath(path) }
        return true
    }

    private func openParentDirectory(
        for path: String,
        createMissing: Bool
    ) throws -> (descriptor: Int32, name: String, createdDirectories: Set<String>) {
        let pieces = try pathPieces(path)
        var current = dup(descriptor.value)
        guard current >= 0 else { throw posixError() }
        var createdDirectories = Set<String>()
        var traversed: [String] = []

        do {
            for component in pieces.dropLast() {
                traversed.append(component)
                var next = component.withCString {
                    openat(current, $0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
                }
                if next < 0, errno == ENOENT, createMissing {
                    let made = component.withCString {
                        mkdirat(current, $0, mode_t(S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH))
                    }
                    if made == 0 {
                        createdDirectories.insert(traversed.joined(separator: "/"))
                    } else if errno != EEXIST {
                        throw posixError()
                    }
                    next = component.withCString {
                        openat(current, $0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
                    }
                }
                guard next >= 0 else {
                    if errno == ELOOP || errno == ENOTDIR { throw NMPUIError.unsafePath(path) }
                    throw posixError()
                }
                close(current)
                current = next
            }
            return (current, pieces.last!, createdDirectories)
        } catch {
            close(current)
            throw error
        }
    }

    private func pathPieces(_ relativePath: String) throws -> [String] {
        let pieces = relativePath.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        guard !relativePath.isEmpty,
              !relativePath.hasPrefix("/"),
              !relativePath.contains("\\"),
              pieces.allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else {
            throw NMPUIError.unsafePath(relativePath)
        }
        return pieces
    }

    private func deeperPathFirst(_ left: String, _ right: String) -> Bool {
        let leftDepth = left.split(separator: "/").count
        let rightDepth = right.split(separator: "/").count
        return leftDepth == rightDepth ? left > right : leftDepth > rightDepth
    }

    private func posixError() -> Error {
        POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
    }
}

private func DarwinOrGlibcWrite(_ descriptor: Int32, _ buffer: UnsafeRawPointer, _ count: Int) -> Int {
#if canImport(Darwin)
    Darwin.write(descriptor, buffer, count)
#else
    Glibc.write(descriptor, buffer, count)
#endif
}
