import Foundation

enum Mode: String, Sendable {
    case online
    case provenanceGrowth = "provenance-growth"
    case restart
}

struct Args: Sendable {
    let mode: Mode
    let relayA: String
    let relayB: String
    let viewer: String
    let followed: String
    let outsider: String
    let writerSecretFile: String
    let storePath: String
    let readyFile: String?
    let settleSeconds: UInt64

    static func parse() throws -> Args {
        var values = Array(CommandLine.arguments.dropFirst())
        guard let modeName = values.first, let mode = Mode(rawValue: modeName) else {
            throw ProbeError.message("missing or unknown mode")
        }
        values.removeFirst()

        var options: [String: String] = [:]
        while !values.isEmpty {
            let key = values.removeFirst()
            guard key.hasPrefix("--"), !values.isEmpty else {
                throw ProbeError.message("\(key) requires a value")
            }
            options[key] = values.removeFirst()
        }

        func required(_ name: String) throws -> String {
            guard let value = options[name] else {
                throw ProbeError.message("missing \(name)")
            }
            return value
        }

        let settle: UInt64
        if let raw = options["--settle-secs"] {
            guard let parsed = UInt64(raw) else {
                throw ProbeError.message("--settle-secs must be an integer")
            }
            settle = parsed
        } else {
            settle = 30
        }

        return Args(
            mode: mode,
            relayA: try required("--relay-a"),
            relayB: try required("--relay-b"),
            viewer: try required("--viewer"),
            followed: try required("--followed"),
            outsider: try required("--outsider"),
            writerSecretFile: try required("--writer-secret-file"),
            storePath: try required("--store"),
            readyFile: options["--ready-file"],
            settleSeconds: settle
        )
    }
}
