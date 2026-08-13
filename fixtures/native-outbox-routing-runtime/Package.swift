// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "OutboxRoutingSwiftConsumer",
    platforms: [.macOS(.v13)],
    dependencies: [.package(path: "Generated/NMP/apple")],
    targets: [
        .executableTarget(
            name: "OutboxRoutingSwiftConsumer",
            dependencies: [.product(name: "NMP", package: "apple")]
        ),
    ]
)
