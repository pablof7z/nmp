// swift-tools-version: 5.10

import PackageDescription

let package = Package(
    name: "nmp-ui",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "nmp-ui", targets: ["NMPUICLI"]),
        .library(name: "NMPUICore", targets: ["NMPUICore"]),
    ],
    targets: [
        .target(
            name: "NMPUICore",
            resources: [.copy("Registry")]
        ),
        .executableTarget(
            name: "NMPUICLI",
            dependencies: ["NMPUICore"]
        ),
        .testTarget(
            name: "NMPUICoreTests",
            dependencies: ["NMPUICore"]
        ),
    ]
)
