import Foundation

public extension ComponentCatalog {
    static func bundled() throws -> ComponentCatalog {
        guard let registryURL = Bundle.module.url(forResource: "registry", withExtension: "json", subdirectory: "Registry") else {
            throw NMPUIError.invalidRegistry("bundled registry.json is missing")
        }
        let registry = try JSONDecoder().decode(ComponentRegistry.self, from: Data(contentsOf: registryURL))
        var templates: [String: String] = [:]
        for component in registry.components {
            for file in component.files where templates[file.source] == nil {
                guard let url = Bundle.module.url(forResource: file.source, withExtension: nil, subdirectory: "Registry/templates") else {
                    throw NMPUIError.invalidRegistry("bundled template \(file.source) is missing")
                }
                templates[file.source] = try String(contentsOf: url, encoding: .utf8)
            }
        }
        return ComponentCatalog(registry: registry, templates: templates)
    }
}
