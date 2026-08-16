// swift-tools-version:6.1
import PackageDescription

let package = Package(
    name: "RelayLabKit",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "RelayLabKit", targets: ["RelayLabKit"]),
        .executable(name: "relay-lab-lifecycle", targets: ["relay-lab-lifecycle"]),
        .executable(name: "relay-lab-nip42", targets: ["relay-lab-nip42"]),
    ],
    dependencies: [
        // BIP-340 Schnorr signing for real NIP-01/NIP-42 event construction --
        // the same operation any real Nostr client performs. Only used here
        // by the LAB CONTROLLER (to seed events and to drive the NIP-42
        // client handshake); never by the relay under test, which is the
        // real strfry binary.
        .package(url: "https://github.com/21-DOT-DEV/swift-secp256k1", exact: "0.23.2"),
    ],
    targets: [
        .target(
            name: "RelayLabKit",
            dependencies: [
                .product(name: "P256K", package: "swift-secp256k1")
            ]
        ),
        .executableTarget(
            name: "relay-lab-lifecycle",
            dependencies: ["RelayLabKit"]
        ),
        .executableTarget(
            name: "relay-lab-nip42",
            dependencies: ["RelayLabKit"]
        ),
    ]
)
