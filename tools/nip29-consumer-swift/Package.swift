// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "NIP29Consumer",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "NIP29Consumer", targets: ["NIP29Consumer"]),
    ],
    dependencies: [
        .package(path: "../../Packages/NMP"),
    ],
    targets: [
        .executableTarget(
            name: "NIP29Consumer",
            dependencies: [.product(name: "NMP", package: "nmp")]
        ),
    ]
)
