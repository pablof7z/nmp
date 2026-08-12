// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "NMPNativeCLIConsumer",
    platforms: [.macOS(.v13)],
    dependencies: [
        .package(path: "Generated/NMP/apple"),
    ],
    targets: [
        .executableTarget(
            name: "NMPNativeCLIConsumer",
            dependencies: [.product(name: "NMP", package: "apple")]
        ),
    ]
)
