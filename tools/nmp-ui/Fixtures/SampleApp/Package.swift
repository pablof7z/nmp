// swift-tools-version: 5.10

import PackageDescription

let package = Package(
    name: "NMPUISampleApp",
    platforms: [.macOS(.v13)],
    dependencies: [
        .package(name: "NMP", path: "../../../../Packages/NMP"),
    ],
    targets: [
        .executableTarget(
            name: "NMPUISampleApp",
            dependencies: [
                .product(name: "NMPContent", package: "NMP"),
                .product(name: "NMPUI", package: "NMP"),
            ],
            path: ".",
            exclude: ["README.md", ".nmp-ui-lock.json"],
            sources: ["Sources", "Components"]
        ),
    ]
)
