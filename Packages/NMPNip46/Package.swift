// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "NMPNip46",
    platforms: [
        .iOS(.v16),
        .macOS(.v13),
    ],
    products: [
        .library(name: "NMPNip46", targets: ["NMPNip46"]),
    ],
    dependencies: [
        .package(path: "../NMP"),
    ],
    targets: [
        .binaryTarget(
            name: "nmp_nip46_ffiFFI",
            path: "NMPNip46.xcframework"
        ),
        .target(
            name: "NMPNip46FFI",
            dependencies: [
                "nmp_nip46_ffiFFI",
                .product(name: "NMPComponentCore", package: "NMP"),
            ]
        ),
        .target(
            name: "NMPNip46",
            dependencies: [
                "NMPNip46FFI",
                .product(name: "NMP", package: "NMP"),
            ],
            linkerSettings: [
                .linkedFramework(
                    "Security",
                    .when(platforms: [.iOS, .macOS])
                ),
            ]
        ),
        .testTarget(
            name: "NMPNip46Tests",
            dependencies: ["NMPNip46"]
        ),
    ]
)
