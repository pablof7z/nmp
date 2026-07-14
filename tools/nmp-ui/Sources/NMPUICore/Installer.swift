import Foundation

public final class NMPUIInstaller {
    private let catalog: ComponentCatalog
    private let componentsByName: [String: RegistryComponent]
    private let fileSystem: ProjectFileSystem
    private let merger: ThreeWayMerging

    public convenience init(catalog: ComponentCatalog, projectRoot: URL) throws {
        try self.init(catalog: catalog, projectRoot: projectRoot, mutationHook: nil)
    }

    init(
        catalog: ComponentCatalog,
        projectRoot: URL,
        mutationHook: ProjectFileSystem.MutationHook?
    ) throws {
        self.catalog = catalog
        self.fileSystem = try ProjectFileSystem(root: projectRoot, mutationHook: mutationHook)
        self.merger = GitThreeWayMerger()
        self.componentsByName = try Self.validate(catalog: catalog, root: projectRoot)
    }

    public func list() -> OperationReport {
        OperationReport(lines: componentsByName.values.sorted { $0.name < $1.name }.map {
            "\($0.name)\t\($0.version)\t\($0.summary)"
        })
    }

    public func view(_ name: String) throws -> OperationReport {
        let closure = try dependencyClosure(for: name)
        guard let requested = componentsByName[name] else { throw NMPUIError.unknownComponent(name) }
        var lines = [
            "name: \(requested.name)",
            "version: \(requested.version)",
            "summary: \(requested.summary)",
            "dependency closure: \(closure.map(\.name).joined(separator: " -> "))",
            "",
        ]
        for component in closure {
            for file in component.files.sorted(by: { $0.destination < $1.destination }) {
                lines.append("--- \(file.destination) [\(component.name):\(file.role)]")
                lines.append(try catalog.template(named: file.source))
            }
        }
        return OperationReport(lines: lines)
    }

    public func add(_ name: String) throws -> OperationReport {
        let closure = try dependencyClosure(for: name)
        let loadedLock = try fileSystem.loadLockSnapshot(registryVersion: catalog.registry.registryVersion)
        var lock = loadedLock.state
        var writes: [(String, String)] = []
        var expected: [String: ProjectFileSystem.FileState] = [
            ProjectFileSystem.lockPath: loadedLock.file,
        ]
        var lines: [String] = []

        for component in closure {
            if let installed = lock.components[component.name] {
                for path in installed.files.keys.sorted() where try !fileSystem.exists(path) {
                    throw NMPUIError.missingManagedFile(path)
                }
                lines.append("already installed \(component.name)@\(installed.version)")
                continue
            }

            var lockedFiles: [String: LockedFile] = [:]
            for file in component.files.sorted(by: { $0.destination < $1.destination }) {
                if try fileSystem.exists(file.destination) { throw NMPUIError.collision(file.destination) }
                expected[file.destination] = .absent
                let content = try catalog.template(named: file.source)
                writes.append((file.destination, content))
                lockedFiles[file.destination] = LockedFile(
                    upstreamHash: SHA256.digest(content),
                    upstreamBase: content
                )
            }
            lock.components[component.name] = LockedComponent(
                version: component.version,
                dependencies: component.dependencies.sorted(),
                files: lockedFiles
            )
            lines.append("installed \(component.name)@\(component.version)")
        }

        lock.registryVersion = catalog.registry.registryVersion
        try fileSystem.commit(
            writes: try fileSystem.transactionWrites(writes, lock: lock),
            removals: [],
            expected: expected
        )
        return OperationReport(lines: lines)
    }

    public func diff(_ name: String) throws -> OperationReport {
        let lock = try fileSystem.loadLock(registryVersion: catalog.registry.registryVersion)
        guard let installed = lock.components[name] else { throw NMPUIError.notInstalled(name) }
        var lines: [String] = []
        var changed = false
        for (path, lockedFile) in installed.files.sorted(by: { $0.key < $1.key }) {
            guard try fileSystem.exists(path) else {
                changed = true
                lines.append("D \(path)")
                continue
            }
            let local = try fileSystem.read(path)
            if local == lockedFile.upstreamBase {
                lines.append("= \(path)")
            } else {
                changed = true
                lines.append("M \(path)")
                lines.append(contentsOf: Self.readableDiff(base: lockedFile.upstreamBase, local: local))
            }
        }
        return OperationReport(lines: lines, hasDifferences: changed)
    }

    public func update(_ name: String) throws -> OperationReport {
        let closure = try dependencyClosure(for: name)
        let loadedLock = try fileSystem.loadLockSnapshot(registryVersion: catalog.registry.registryVersion)
        var lock = loadedLock.state
        for component in closure where lock.components[component.name] == nil {
            throw NMPUIError.notInstalled(component.name)
        }
        var conflicts = try fileSystem.loadConflicts()
        let unresolved = closure.compactMap { conflicts.components[$0.name]?.paths }.flatMap { $0 }
        if !unresolved.isEmpty { throw NMPUIError.updateConflict(unresolved) }

        var writes: [String: String] = [:]
        var removals = Set<String>()
        var expected: [String: ProjectFileSystem.FileState] = [
            ProjectFileSystem.lockPath: loadedLock.file,
        ]
        var nextEntries: [String: LockedComponent] = [:]
        var conflictPathsByComponent: [String: [String]] = [:]

        for component in closure {
            guard let installed = lock.components[component.name] else {
                throw NMPUIError.notInstalled(component.name)
            }

            var currentFiles: [String: RegistryFile] = [:]
            for file in component.files {
                guard currentFiles[file.destination] == nil else {
                    throw NMPUIError.invalidRegistry(
                        "duplicate destination \(file.destination) in component \(component.name)"
                    )
                }
                currentFiles[file.destination] = file
            }
            let allPaths = Set(installed.files.keys).union(currentFiles.keys)
            var nextFiles: [String: LockedFile] = [:]

            for path in allPaths.sorted() {
                let old = installed.files[path]
                let current = currentFiles[path]
                switch (old, current) {
                case let (.some(oldFile), .some(currentFile)):
                    guard try fileSystem.exists(path) else { throw NMPUIError.missingManagedFile(path) }
                    let local = try fileSystem.read(path)
                    expected[path] = .present(Data(local.utf8))
                    let upstream = try catalog.template(named: currentFile.source)
                    let merged = try merger.merge(local: local, base: oldFile.upstreamBase, upstream: upstream)
                    writes[path] = merged.content
                    if merged.hasConflicts { conflictPathsByComponent[component.name, default: []].append(path) }
                    nextFiles[path] = LockedFile(
                        upstreamHash: SHA256.digest(upstream), upstreamBase: upstream
                    )

                case let (.some(oldFile), .none):
                    guard try fileSystem.exists(path) else { throw NMPUIError.missingManagedFile(path) }
                    let local = try fileSystem.read(path)
                    expected[path] = .present(Data(local.utf8))
                    let merged = try merger.merge(local: local, base: oldFile.upstreamBase, upstream: "")
                    if merged.hasConflicts {
                        writes[path] = merged.content
                        conflictPathsByComponent[component.name, default: []].append(path)
                    } else if merged.content.isEmpty {
                        removals.insert(path)
                    } else {
                        writes[path] = merged.content
                    }

                case let (.none, .some(currentFile)):
                    if try fileSystem.exists(path) { throw NMPUIError.collision(path) }
                    expected[path] = .absent
                    let upstream = try catalog.template(named: currentFile.source)
                    writes[path] = upstream
                    nextFiles[path] = LockedFile(
                        upstreamHash: SHA256.digest(upstream), upstreamBase: upstream
                    )

                case (.none, .none):
                    break
                }
            }
            nextEntries[component.name] = LockedComponent(
                version: component.version,
                dependencies: component.dependencies.sorted(),
                files: nextFiles
            )
        }

        if !conflictPathsByComponent.isEmpty {
            let conflictPaths = conflictPathsByComponent.values.flatMap { $0 }.sorted()
            for path in conflictPaths {
                if let content = writes[path], let expectedState = expected[path] {
                    try fileSystem.write(content, to: path, expected: expectedState)
                }
            }
            for component in closure {
                guard let paths = conflictPathsByComponent[component.name],
                      let installed = lock.components[component.name] else { continue }
                conflicts.components[component.name] = ConflictedComponent(
                    fromVersion: installed.version,
                    toVersion: component.version,
                    paths: paths.sorted()
                )
            }
            try fileSystem.saveConflicts(conflicts)
            throw NMPUIError.updateConflict(conflictPaths)
        }

        for (name, entry) in nextEntries { lock.components[name] = entry }
        lock.registryVersion = catalog.registry.registryVersion
        try fileSystem.commit(
            writes: try fileSystem.transactionWrites(writes, lock: lock),
            removals: removals,
            expected: expected
        )
        return OperationReport(lines: closure.map { "updated \($0.name)@\($0.version)" })
    }

    public func dependencyClosure(for name: String) throws -> [RegistryComponent] {
        guard componentsByName[name] != nil else { throw NMPUIError.unknownComponent(name) }
        var visiting = Set<String>()
        var visited = Set<String>()
        var result: [RegistryComponent] = []

        func visit(_ componentName: String) throws {
            if visited.contains(componentName) { return }
            if !visiting.insert(componentName).inserted {
                throw NMPUIError.invalidRegistry("dependency cycle at \(componentName)")
            }
            guard let component = componentsByName[componentName] else {
                throw NMPUIError.invalidRegistry("unknown dependency \(componentName)")
            }
            for dependency in component.dependencies.sorted() { try visit(dependency) }
            visiting.remove(componentName)
            visited.insert(componentName)
            result.append(component)
        }

        try visit(name)
        try Self.validatePlannedDestinations(result)
        return result
    }

    private static func validate(catalog: ComponentCatalog, root: URL) throws -> [String: RegistryComponent] {
        guard catalog.registry.schemaVersion == 1 else {
            throw NMPUIError.invalidRegistry("unsupported schema \(catalog.registry.schemaVersion)")
        }
        var components: [String: RegistryComponent] = [:]
        var destinations: [String: String] = [:]
        let checker = try ProjectFileSystem(root: root)
        for component in catalog.registry.components {
            guard !component.name.isEmpty,
                  component.name.allSatisfy({ $0.isLowercase || $0.isNumber || $0 == "-" }) else {
                throw NMPUIError.invalidRegistry("invalid component name \(component.name)")
            }
            guard components[component.name] == nil else {
                throw NMPUIError.invalidRegistry("duplicate component \(component.name)")
            }
            var componentDestinations = Set<String>()
            for file in component.files {
                guard !ProjectFileSystem.reservedPaths.contains(file.destination) else {
                    throw NMPUIError.invalidRegistry("reserved destination \(file.destination)")
                }
                _ = try checker.resolve(file.source)
                _ = try checker.resolve(file.destination)
                _ = try catalog.template(named: file.source)
                guard componentDestinations.insert(file.destination).inserted else {
                    throw NMPUIError.invalidRegistry(
                        "duplicate destination \(file.destination) in component \(component.name)"
                    )
                }
                if let owner = destinations[file.destination], owner != component.name {
                    throw NMPUIError.invalidRegistry("destination \(file.destination) belongs to both \(owner) and \(component.name)")
                }
                destinations[file.destination] = component.name
            }
            components[component.name] = component
        }
        for component in components.values {
            for dependency in component.dependencies where components[dependency] == nil {
                throw NMPUIError.invalidRegistry("\(component.name) depends on unknown component \(dependency)")
            }
        }
        return components
    }

    private static func validatePlannedDestinations(_ closure: [RegistryComponent]) throws {
        var owners: [String: String] = [:]
        for component in closure {
            for file in component.files {
                guard !ProjectFileSystem.reservedPaths.contains(file.destination) else {
                    throw NMPUIError.invalidRegistry("reserved destination \(file.destination)")
                }
                if let owner = owners[file.destination] {
                    throw NMPUIError.invalidRegistry(
                        "planned destination \(file.destination) belongs to both \(owner) and \(component.name)"
                    )
                }
                owners[file.destination] = component.name
            }
        }
    }

    private static func readableDiff(base: String, local: String) -> [String] {
        var lines = ["--- upstream base", "+++ local"]
        lines.append(contentsOf: base.split(separator: "\n", omittingEmptySubsequences: false).map { "-\($0)" })
        lines.append(contentsOf: local.split(separator: "\n", omittingEmptySubsequences: false).map { "+\($0)" })
        return lines
    }
}
