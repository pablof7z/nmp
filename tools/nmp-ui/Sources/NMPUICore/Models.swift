import Foundation

public struct ComponentRegistry: Codable, Equatable {
    public let schemaVersion: Int
    public let registryVersion: String
    public let components: [RegistryComponent]

    public init(schemaVersion: Int = 1, registryVersion: String, components: [RegistryComponent]) {
        self.schemaVersion = schemaVersion
        self.registryVersion = registryVersion
        self.components = components
    }
}

public struct RegistryComponent: Codable, Equatable {
    public let name: String
    public let version: String
    public let summary: String
    public let dependencies: [String]
    public let files: [RegistryFile]

    public init(
        name: String,
        version: String,
        summary: String,
        dependencies: [String] = [],
        files: [RegistryFile]
    ) {
        self.name = name
        self.version = version
        self.summary = summary
        self.dependencies = dependencies
        self.files = files
    }
}

public struct RegistryFile: Codable, Equatable {
    public let source: String
    public let destination: String
    public let role: String

    public init(source: String, destination: String, role: String = "source") {
        self.source = source
        self.destination = destination
        self.role = role
    }
}

public struct ComponentCatalog {
    public let registry: ComponentRegistry
    private let templates: [String: String]

    public init(registry: ComponentRegistry, templates: [String: String]) {
        self.registry = registry
        self.templates = templates
    }

    public func template(named name: String) throws -> String {
        guard let template = templates[name] else {
            throw NMPUIError.invalidRegistry("template \(name) is missing")
        }
        return template
    }
}

public struct OperationReport: Equatable {
    public let lines: [String]
    public let hasDifferences: Bool

    public init(lines: [String], hasDifferences: Bool = false) {
        self.lines = lines
        self.hasDifferences = hasDifferences
    }

    public var text: String { lines.joined(separator: "\n") }
}

public enum NMPUIError: Error, CustomStringConvertible, Equatable {
    case usage(String)
    case unknownComponent(String)
    case invalidRegistry(String)
    case unsafePath(String)
    case collision(String)
    case notInstalled(String)
    case missingManagedFile(String)
    case mergeFailed(String)
    case updateConflict([String])

    public var description: String {
        switch self {
        case .usage(let message): return message
        case .unknownComponent(let name): return "unknown component: \(name)"
        case .invalidRegistry(let message): return "invalid registry: \(message)"
        case .unsafePath(let path): return "unsafe path: \(path)"
        case .collision(let path): return "refusing to overwrite unmanaged path: \(path)"
        case .notInstalled(let name): return "component is not installed: \(name)"
        case .missingManagedFile(let path): return "managed file is missing: \(path)"
        case .mergeFailed(let message): return "three-way merge failed: \(message)"
        case .updateConflict(let paths):
            return "update has conflicts in: \(paths.sorted().joined(separator: ", "))"
        }
    }
}
