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
        // C2's ONLINE half. The claim under test is what survives a
        // RESTART, and a second `NMPEngine` built inside the process that
        // just filled the store is not a restart -- the pages, the
        // allocator and every decoded row are still in that address
        // space. This executable fills the store and quits; the scenario
        // waits for it to be really gone before opening the store again,
        // so the read it then asserts on is genuinely cold. Needs NMP
        // only -- the relay's lifecycle stays the parent's job.
        .executableTarget(
            name: "canary-c2-warmer",
            dependencies: [
                .product(name: "NMP", package: "NMP")
            ]
        ),
        // C17's measured half. Memory footprint, open file descriptors and
        // live thread count are properties of a PROCESS, and issue #1796 is
        // the standing proof that a process-wide measurement inside a shared
        // test binary cannot tell the thing under test from everything else
        // running beside it. This executable is a process whose only job is
        // the churn, so its numbers are attributable. Like
        // `canary-c9-publisher` it needs NMP only -- the relay's lifecycle
        // and seeding stay the parent test's job.
        .executableTarget(
            name: "canary-c17-churner",
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
