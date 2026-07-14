import Foundation

#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

struct ProjectFileSystem {
    static let lockPath = ".nmp-ui-lock.json"
    static let conflictPath = ".nmp-ui-conflicts.json"
    static let reservedPaths: Set<String> = [lockPath, conflictPath]

    private struct CreatedDirectory: Hashable {
        let path: String
        let device: UInt64
        let inode: UInt64
    }

    struct FileState {
        let data: Data?

        static var absent: FileState { FileState(data: nil) }
        static func present(_ data: Data) -> FileState { FileState(data: data) }
    }

    enum MutationPoint: Equatable {
        case write(String)
        case remove(String)
        case afterSwap(String)
        case afterQuarantine(String)
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
        let target: (descriptor: Int32, name: String)
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
        let target: (descriptor: Int32, name: String)
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
        try write(Data(content.utf8), to: path)
    }

    func write(_ data: Data, to path: String) throws {
        let expected = FileState(data: try readDataIfPresent(path))
        try write(data, to: path, expected: expected)
    }

    func write(_ content: String, to path: String, expected: FileState) throws {
        try write(Data(content.utf8), to: path, expected: expected)
    }

    func remove(_ path: String) throws {
        let expected = FileState(data: try readDataIfPresent(path))
        try mutationHook?(.remove(path))
        var createdDirectories = Set<CreatedDirectory>()
        try replace(
            path,
            expected: expected,
            desired: .absent,
            createdDirectories: &createdDirectories
        )
    }

    func commit(
        writes: [String: Data],
        removals: Set<String>,
        expected: [String: FileState]
    ) throws {
        let targets = Set(writes.keys).union(removals)
        guard Set(expected.keys) == targets else {
            throw NMPUIError.invalidRegistry("mutation plan does not cover its exact target set")
        }

        var createdDirectories = Set<CreatedDirectory>()
        var completed: [(path: String, before: FileState, after: FileState)] = []
        do {
            for path in removals.sorted() {
                try mutationHook?(.remove(path))
                try replace(
                    path,
                    expected: expected[path]!,
                    desired: .absent,
                    createdDirectories: &createdDirectories
                )
                completed.append((path, expected[path]!, .absent))
            }
            let ordinaryWrites = writes.keys.filter { $0 != Self.lockPath }.sorted()
            for path in ordinaryWrites {
                let desired = FileState.present(writes[path]!)
                try mutationHook?(.write(path))
                try replace(
                    path,
                    expected: expected[path]!,
                    desired: desired,
                    createdDirectories: &createdDirectories
                )
                completed.append((path, expected[path]!, desired))
            }
            if let lock = writes[Self.lockPath] {
                let desired = FileState.present(lock)
                try mutationHook?(.write(Self.lockPath))
                try replace(
                    Self.lockPath,
                    expected: expected[Self.lockPath]!,
                    desired: desired,
                    createdDirectories: &createdDirectories
                )
                completed.append((Self.lockPath, expected[Self.lockPath]!, desired))
            }
        } catch {
            let originalError = error
            var rollbackErrors: [String] = []
            for mutation in completed.reversed() {
                do {
                    try replace(
                        mutation.path,
                        expected: mutation.after,
                        desired: mutation.before,
                        createdDirectories: &createdDirectories
                    )
                } catch {
                    rollbackErrors.append("\(mutation.path): \(error)")
                }
            }
            for directory in createdDirectories.sorted(by: deeperPathFirst) {
                do {
                    try removeCreatedDirectoryIfUnchanged(directory)
                } catch {
                    rollbackErrors.append("\(directory.path): \(error)")
                }
            }
            guard rollbackErrors.isEmpty else {
                throw NMPUIError.transactionFailed(
                    "mutation failed with \(originalError); rollback refused or failed: "
                        + rollbackErrors.joined(separator: "; ")
                )
            }
            throw originalError
        }
    }

    private func write(_ data: Data, to path: String, expected: FileState) throws {
        try mutationHook?(.write(path))
        var createdDirectories = Set<CreatedDirectory>()
        do {
            try replace(
                path,
                expected: expected,
                desired: .present(data),
                createdDirectories: &createdDirectories
            )
        } catch {
            let originalError = error
            var cleanupErrors: [String] = []
            for directory in createdDirectories.sorted(by: deeperPathFirst) {
                do {
                    try removeCreatedDirectoryIfUnchanged(directory)
                } catch {
                    cleanupErrors.append("\(directory.path): \(error)")
                }
            }
            guard cleanupErrors.isEmpty else {
                throw NMPUIError.transactionFailed(
                    "mutation failed with \(originalError); cleanup refused or failed: "
                        + cleanupErrors.joined(separator: "; ")
                )
            }
            throw originalError
        }
    }

    private func replace(
        _ path: String,
        expected: FileState,
        desired: FileState,
        createdDirectories: inout Set<CreatedDirectory>
    ) throws {
        switch (expected.data, desired.data) {
        case (nil, nil):
            return
        case (nil, .some(let data)):
            try createWithoutReplacing(
                data,
                at: path,
                createdDirectories: &createdDirectories
            )
        case (.some(let expectedData), .some(let desiredData)):
            if expectedData == desiredData {
                try verifyCurrent(path, expected: expectedData)
            } else {
                try exchangeIfUnchanged(
                    path,
                    expected: expectedData,
                    desired: desiredData,
                    createdDirectories: &createdDirectories
                )
            }
        case (.some(let expectedData), nil):
            try removeIfUnchanged(path, expected: expectedData)
        }
    }

    private func createWithoutReplacing(
        _ data: Data,
        at path: String,
        createdDirectories: inout Set<CreatedDirectory>
    ) throws {
        let target = try openParentDirectory(
            for: path,
            createMissing: true,
            createdDirectories: &createdDirectories
        )
        defer { close(target.descriptor) }
        let temporary = try createTemporaryFile(data, parent: target.descriptor)
        defer { close(temporary.descriptor) }
        var temporaryExists = true
        defer {
            if temporaryExists {
                temporary.name.withCString { _ = unlinkat(target.descriptor, $0, 0) }
            }
        }

        let result = atomicRename(
            oldParent: target.descriptor,
            oldName: temporary.name,
            newParent: target.descriptor,
            newName: target.name,
            flags: UInt32(RENAME_EXCL)
        )
        guard result == 0 else {
            if errno == EEXIST { throw NMPUIError.collision(path) }
            throw posixError()
        }
        temporaryExists = false
    }

    private func exchangeIfUnchanged(
        _ path: String,
        expected: Data,
        desired: Data,
        createdDirectories: inout Set<CreatedDirectory>
    ) throws {
        let target = try openParentDirectory(
            for: path,
            createMissing: false,
            createdDirectories: &createdDirectories
        )
        defer { close(target.descriptor) }
        let temporary = try createTemporaryFile(desired, parent: target.descriptor)
        defer { close(temporary.descriptor) }
        var temporaryExists = true
        defer {
            if temporaryExists {
                temporary.name.withCString { _ = unlinkat(target.descriptor, $0, 0) }
            }
        }

        let exchanged = atomicRename(
            oldParent: target.descriptor,
            oldName: temporary.name,
            newParent: target.descriptor,
            newName: target.name,
            flags: UInt32(RENAME_SWAP)
        )
        guard exchanged == 0 else {
            if errno == ENOENT || errno == EEXIST { throw NMPUIError.concurrentModification(path) }
            throw posixError()
        }

        do {
            try mutationHook?(.afterSwap(path))
            let displaced = try readData(parent: target.descriptor, name: temporary.name, path: path)
            guard displaced == expected else {
                temporaryExists = false
                try restoreUnexpectedExchange(
                    parent: target.descriptor,
                    temporaryName: temporary.name,
                    targetName: target.name,
                    proposed: desired,
                    path: path
                )
                throw NMPUIError.concurrentModification(path)
            }

            let removed = temporary.name.withCString { unlinkat(target.descriptor, $0, 0) }
            guard removed == 0 else { throw posixError() }
            temporaryExists = false
        } catch {
            if let nmpError = error as? NMPUIError,
               case .concurrentModification = nmpError {
                throw error
            }
            temporaryExists = false
            try restoreUnexpectedExchange(
                parent: target.descriptor,
                temporaryName: temporary.name,
                targetName: target.name,
                proposed: desired,
                path: path
            )
            throw error
        }
    }

    private func restoreUnexpectedExchange(
        parent: Int32,
        temporaryName: String,
        targetName: String,
        proposed: Data,
        path: String
    ) throws {
        let restored = atomicRename(
            oldParent: parent,
            oldName: temporaryName,
            newParent: parent,
            newName: targetName,
            flags: UInt32(RENAME_SWAP)
        )
        guard restored == 0 else {
            throw NMPUIError.transactionFailed("could not restore concurrently changed \(path)")
        }

        let returned = try readData(parent: parent, name: temporaryName, path: path)
        if returned != proposed {
            let preservedNewest = atomicRename(
                oldParent: parent,
                oldName: temporaryName,
                newParent: parent,
                newName: targetName,
                flags: UInt32(RENAME_SWAP)
            )
            guard preservedNewest == 0 else {
                throw NMPUIError.transactionFailed("could not preserve newest concurrent bytes for \(path)")
            }
        }
        let removed = temporaryName.withCString { unlinkat(parent, $0, 0) }
        guard removed == 0 else {
            throw NMPUIError.transactionFailed("could not clean exchange state for \(path)")
        }
    }

    private func removeIfUnchanged(_ path: String, expected: Data) throws {
        var ignoredDirectories = Set<CreatedDirectory>()
        let target = try openParentDirectory(
            for: path,
            createMissing: false,
            createdDirectories: &ignoredDirectories
        )
        defer { close(target.descriptor) }
        let quarantine = ".nmp-ui-old-\(UUID().uuidString)"
        let moved = atomicRename(
            oldParent: target.descriptor,
            oldName: target.name,
            newParent: target.descriptor,
            newName: quarantine,
            flags: UInt32(RENAME_EXCL)
        )
        guard moved == 0 else {
            if errno == ENOENT || errno == EEXIST { throw NMPUIError.concurrentModification(path) }
            throw posixError()
        }
        var quarantineExists = true
        defer {
            if quarantineExists {
                quarantine.withCString { _ = unlinkat(target.descriptor, $0, 0) }
            }
        }

        do {
            try mutationHook?(.afterQuarantine(path))
            let displaced = try readData(parent: target.descriptor, name: quarantine, path: path)
            guard displaced == expected else {
                quarantineExists = false
                let restored = atomicRename(
                    oldParent: target.descriptor,
                    oldName: quarantine,
                    newParent: target.descriptor,
                    newName: target.name,
                    flags: UInt32(RENAME_EXCL)
                )
                guard restored == 0 else {
                    throw NMPUIError.transactionFailed(
                        "concurrent bytes preserved at \(quarantine) after \(path) changed again"
                    )
                }
                throw NMPUIError.concurrentModification(path)
            }

            let removed = quarantine.withCString { unlinkat(target.descriptor, $0, 0) }
            guard removed == 0 else { throw posixError() }
            quarantineExists = false
        } catch {
            if let nmpError = error as? NMPUIError,
               case .concurrentModification = nmpError {
                throw error
            }
            quarantineExists = false
            let restored = atomicRename(
                oldParent: target.descriptor,
                oldName: quarantine,
                newParent: target.descriptor,
                newName: target.name,
                flags: UInt32(RENAME_EXCL)
            )
            guard restored == 0 else { throw NMPUIError.transactionFailed("could not restore \(path)") }
            throw error
        }
    }

    private func verifyCurrent(_ path: String, expected: Data) throws {
        let current: Data
        do {
            current = try readData(path)
        } catch let error as POSIXError where error.code == .ENOENT {
            throw NMPUIError.concurrentModification(path)
        }
        guard current == expected else { throw NMPUIError.concurrentModification(path) }
    }

    private func createTemporaryFile(
        _ data: Data,
        parent: Int32
    ) throws -> (name: String, descriptor: Int32) {
        let name = ".nmp-ui-tmp-\(UUID().uuidString)"
        let fileDescriptor = name.withCString {
            openat(
                parent,
                $0,
                O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
                mode_t(S_IRUSR | S_IWUSR | S_IRGRP | S_IROTH)
            )
        }
        guard fileDescriptor >= 0 else { throw posixError() }
        do {
            try data.withUnsafeBytes { bytes in
                guard var base = bytes.baseAddress else { return }
                var remaining = bytes.count
                while remaining > 0 {
                    let count = DarwinOrGlibcWrite(fileDescriptor, base, remaining)
                    guard count >= 0 else { throw posixError() }
                    remaining -= count
                    base = base.advanced(by: count)
                }
            }
            guard fsync(fileDescriptor) == 0 else { throw posixError() }
            return (name, fileDescriptor)
        } catch {
            close(fileDescriptor)
            name.withCString { _ = unlinkat(parent, $0, 0) }
            throw error
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

    private func removeCreatedDirectoryIfUnchanged(_ directory: CreatedDirectory) throws {
        let path = directory.path
        let target: (descriptor: Int32, name: String)
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
        guard metadata.st_mode & S_IFMT == S_IFDIR,
              UInt64(metadata.st_dev) == directory.device,
              UInt64(metadata.st_ino) == directory.inode else {
            throw NMPUIError.transactionFailed("created directory changed before cleanup: \(path)")
        }
        let result = target.name.withCString { unlinkat(target.descriptor, $0, AT_REMOVEDIR) }
        if result != 0 {
            if errno == ENOTEMPTY || errno == EEXIST { return }
            if errno == ENOENT { return }
            throw posixError()
        }
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
    ) throws -> (descriptor: Int32, name: String) {
        var ignored = Set<CreatedDirectory>()
        return try openParentDirectory(
            for: path,
            createMissing: createMissing,
            createdDirectories: &ignored
        )
    }

    private func openParentDirectory(
        for path: String,
        createMissing: Bool,
        createdDirectories: inout Set<CreatedDirectory>
    ) throws -> (descriptor: Int32, name: String) {
        let pieces = try pathPieces(path)
        var current = dup(descriptor.value)
        guard current >= 0 else { throw posixError() }
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
                        var metadata = stat()
                        let status = component.withCString {
                            fstatat(current, $0, &metadata, AT_SYMLINK_NOFOLLOW)
                        }
                        guard status == 0, metadata.st_mode & S_IFMT == S_IFDIR else {
                            component.withCString { _ = unlinkat(current, $0, AT_REMOVEDIR) }
                            throw status == 0 ? NMPUIError.unsafePath(path) : posixError()
                        }
                        createdDirectories.insert(CreatedDirectory(
                            path: traversed.joined(separator: "/"),
                            device: UInt64(metadata.st_dev),
                            inode: UInt64(metadata.st_ino)
                        ))
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
            return (current, pieces.last!)
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

    private func deeperPathFirst(_ left: CreatedDirectory, _ right: CreatedDirectory) -> Bool {
        let leftDepth = left.path.split(separator: "/").count
        let rightDepth = right.path.split(separator: "/").count
        return leftDepth == rightDepth ? left.path > right.path : leftDepth > rightDepth
    }

    private func posixError() -> Error {
        POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
    }

    private func atomicRename(
        oldParent: Int32,
        oldName: String,
        newParent: Int32,
        newName: String,
        flags: UInt32
    ) -> Int32 {
#if canImport(Darwin)
        oldName.withCString { oldPointer in
            newName.withCString { newPointer in
                renameatx_np(oldParent, oldPointer, newParent, newPointer, flags)
            }
        }
#else
        errno = ENOTSUP
        return -1
#endif
    }
}

private func DarwinOrGlibcWrite(_ descriptor: Int32, _ buffer: UnsafeRawPointer, _ count: Int) -> Int {
#if canImport(Darwin)
    Darwin.write(descriptor, buffer, count)
#else
    Glibc.write(descriptor, buffer, count)
#endif
}
