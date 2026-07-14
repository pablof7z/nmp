import Foundation
import NMPUICore

#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

private let usage = """
Usage: nmp-ui [--root PATH] <command> [component]

Commands:
  list                         List bundled source components.
  view <component>             Print metadata, dependency closure, and source.
  add <component>              Install source and its dependency closure.
  diff <component>             Compare app-owned source with its locked base.
  update <component>           Three-way update installed source.
"""

private struct Arguments {
    let root: URL
    let command: String
    let component: String?

    init(_ raw: [String]) throws {
        var values = raw
        var root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
        if let index = values.firstIndex(of: "--root") {
            guard values.indices.contains(index + 1) else { throw NMPUIError.usage("--root requires a path") }
            root = URL(fileURLWithPath: values[index + 1], isDirectory: true)
            values.removeSubrange(index...index + 1)
        }
        guard let command = values.first else { throw NMPUIError.usage(usage) }
        let remaining = Array(values.dropFirst())
        switch command {
        case "list":
            guard remaining.isEmpty else { throw NMPUIError.usage("list takes no component") }
        case "view", "add", "diff", "update":
            guard remaining.count == 1 else { throw NMPUIError.usage("\(command) requires exactly one component") }
        case "help", "--help", "-h":
            throw NMPUIError.usage(usage)
        default:
            throw NMPUIError.usage("unknown command: \(command)\n\n\(usage)")
        }
        self.root = root
        self.command = command
        self.component = remaining.first
    }
}

do {
    let arguments = try Arguments(Array(CommandLine.arguments.dropFirst()))
    let installer = try NMPUIInstaller(catalog: .bundled(), projectRoot: arguments.root)
    let report: OperationReport
    switch arguments.command {
    case "list": report = installer.list()
    case "view": report = try installer.view(arguments.component!)
    case "add": report = try installer.add(arguments.component!)
    case "diff": report = try installer.diff(arguments.component!)
    case "update": report = try installer.update(arguments.component!)
    default: fatalError("validated command was not dispatched")
    }
    if !report.text.isEmpty { print(report.text) }
    if report.hasDifferences { exit(1) }
} catch let error as NMPUIError {
    fputs("nmp-ui: \(error.description)\n", stderr)
    exit(error.description == usage ? 0 : 1)
} catch {
    fputs("nmp-ui: \(error)\n", stderr)
    exit(1)
}
