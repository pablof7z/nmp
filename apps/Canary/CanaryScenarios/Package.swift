// swift-tools-version:5.9
// The Canary's real-relay scenario suite (docs/internals/canary.md's
// C1-C18). A test-only package on purpose: nothing here is a library
// product anything else consumes, so there is nothing to name except the
// test target itself. `RelayLabKit` stays free of any NMP dependency
// (it is a generic relay-lifecycle controller, reusable outside NMP
// entirely); this package is the one place that is both NMP-aware and
// relay-lab-aware, which is exactly what a scenario has to be.
//
// `swift test` from THIS directory is the whole entry point -- no
// xcodegen, no xcodebuild, no simulator. See README.md for the two
// prerequisites (the NMP xcframework; strfry) and what happens when
// either is missing.
import PackageDescription

let package = Package(
    name: "CanaryScenarios",
    platforms: [.macOS(.v13)],
    dependencies: [
        .package(path: "../RelayLabKit"),
        .package(path: "../../../Packages/NMP"),
    ],
    targets: [
        // C9's real-process-death half. `Foundation.Process`/`kill -9`
        // proves nothing about crash safety against an in-process
        // `Engine` drop -- cleanup still runs. This executable is the
        // thing that actually gets `kill -9`ed, in a separate OS
        // process, by C9CrashDuringPublicationTests. It only needs NMP
        // (it talks to relay URLs directly over the wire); it has no
        // RelayLabKit dependency -- lifecycle control of the RELAY is
        // the parent test's job.
        .executableTarget(
            name: "canary-c9-publisher",
            dependencies: [
                .product(name: "NMP", package: "NMP")
            ]
        ),
        .testTarget(
            name: "CanaryScenariosTests",
            dependencies: [
                "RelayLabKit",
                .product(name: "NMP", package: "NMP"),
            ]
        ),
    ]
)
