// The runtime guard `Diagnostics.swift` cannot get from the compiler
// (#1767). Rust's exhaustive-destructure guard in `nmp-ffi/src/convert.rs`
// makes a new engine-side diagnostics fact fail to compile until someone
// decides what happens to it -- but that guard's last line of defense is
// the FFI record, one hop before Swift. `Diagnostics.swift`'s own
// `init(_ ffi:)` initializers are a SEQUENCE OF PLAIN ASSIGNMENTS, and a
// Swift struct initializer never complains about a field it never reads --
// which is exactly how `sessionsRejectedOverCap` went missing at this exact
// hop (#1751/#1756) while every compiler upstream of it stayed green.
//
// `switch` exhaustiveness covers enums only; Swift has no exhaustiveness
// check for a struct initializer, so this cannot be a compile-time guard.
// What it CAN be is a runtime mirror: reflect over the generated `Ffi*`
// value with `Mirror` and assert its stored-property COUNT equals the
// count on the hand-written Swift wrapper `init(_ ffi:)` produces. That is
// exactly the shape of bug this file exists to catch -- an `Ffi*` record
// gains a field and the wrapper's field list quietly stays the same size --
// and it needs no hand-maintained field-name list of its own to do it,
// which is what would make this guard exactly as leaky as the thing it
// checks.
//
// Field-COUNT, not field-name/value equality: `Diagnostics.swift`
// deliberately renames some fields crossing this hop (`authEventId` ->
// `authEventID`), and matching NAMES would either have to special-case
// every rename or produce exactly the kind of false failure that gets a
// guard silenced. A field that is dropped changes the count; a field that
// is renamed does not. That is the one failure mode #1751/#1756 actually
// hit, and the one this test is for.

import XCTest

@testable import NMP
@testable import NMPFFI

final class DiagnosticsFieldMirrorTests: XCTestCase {
    /// Every stored property `Mirror` finds on `ffi`, by label -- used only
    /// to compare COUNTS against the Swift wrapper's own stored properties,
    /// never to compare values or match up names across the hop (see the
    /// file doc for why).
    private static func fieldCount(_ value: Any) -> Int {
        Mirror(reflecting: value).children.count
    }

    /// Asserts the Swift wrapper's `init(_ ffi:)` projected every one of
    /// `ffi`'s stored properties, by comparing stored-property counts on
    /// both sides. Fails with both counts and both type names so a real
    /// drop is diagnosable without re-deriving which field went missing.
    private func assertFullyMirrored<Ffi, Swift_>(
        _ ffi: Ffi,
        _ makeSwift: (Ffi) -> Swift_,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let ffiCount = Self.fieldCount(ffi)
        let swiftCount = Self.fieldCount(makeSwift(ffi))
        XCTAssertEqual(
            swiftCount, ffiCount,
            "\(Swift_.self)(_ ffi: \(Ffi.self)) projects \(swiftCount) field(s) but the FFI "
                + "record carries \(ffiCount) -- a fact is being silently dropped at the "
                + "Swift hop (#1751/#1756's exact bug class)",
            file: file, line: line
        )
    }

    func testKindCountMirrorsEveryFfiField() {
        assertFullyMirrored(FfiKindCount(kind: 1, count: 1), KindCount.init)
    }

    func testLaneCountMirrorsEveryFfiField() {
        assertFullyMirrored(FfiLaneCount(lane: "outbox", count: 1), LaneCount.init)
    }

    func testCoverageIntervalMirrorsEveryFfiField() {
        assertFullyMirrored(FfiCoverageInterval(from: 1, through: 2), CoverageInterval.init)
    }

    func testFilterCoverageMirrorsEveryFfiField() {
        assertFullyMirrored(
            FfiFilterCoverage(filter: "{}", coverage: FfiCoverageInterval(from: 1, through: 2)),
            FilterCoverage.init
        )
    }

    func testRelayDiagnosticsMirrorsEveryFfiField() {
        let ffi = FfiRelayDiagnostics(
            relay: "wss://mirror-fixture.example",
            authenticateAs: nil,
            wireSubCount: 1,
            subscriptionBudget: 1,
            subscriptionsRefused: 1,
            subidLengthLimit: 1,
            subidLengthRejectsOurIds: false,
            authorsServed: 1,
            byLane: [],
            filters: [],
            eventsByKind: [],
            coverage: [],
            nip11SupportedNips: nil,
            nip11DocumentRevision: nil,
            nip11Freshness: nil,
            nip11LastError: nil,
            nip77Advertisement: "x",
            nip77Behavior: "x",
            nip77Handoff: "x"
        )
        assertFullyMirrored(ffi, RelayDiagnostics.init)
    }

    func testAuthDiagnosticsMirrorsEveryFfiField() {
        let ffi = FfiAuthDiagnostics(
            relay: "wss://mirror-fixture.example",
            authenticateAs: nil,
            transportGeneration: 1,
            epochSequence: nil,
            challengeDescriptor: nil,
            phase: .awaitingChallenge,
            policyBound: false,
            signerBound: false,
            authEventId: nil
        )
        assertFullyMirrored(ffi, AuthDiagnostics.init)
    }

    func testStalledWriteMirrorsEveryFfiField() {
        let ffi = FfiStalledWrite(id: "x", stage: .unroutable, detail: "waiting", stalledSince: 1)
        assertFullyMirrored(ffi, StalledWrite.init)
    }

    func testStalledWriteTotalsMirrorsEveryFfiField() {
        let ffi = FfiStalledWriteTotals(
            unroutable: 1, unsignable: 1, undeliverable: 1, omittedDetails: 1, detailLimit: 1
        )
        assertFullyMirrored(ffi, StalledWriteTotals.init)
    }

    /// The one that actually broke (#1751/#1756): `DiagnosticsSnapshot` is
    /// where `sessionsRejectedOverCap` was dropped by a plain assignment
    /// sequence that never complained.
    func testDiagnosticsSnapshotMirrorsEveryFfiField() {
        let ffi = FfiDiagnosticsSnapshot(
            relays: [],
            authSessions: [],
            uncoveredAuthorCount: 1,
            droppedMergeRules: [],
            sessionsRejectedOverCap: 1,
            sessionsRefusedBySubscriptionBudget: 1,
            storeDegraded: nil,
            transportDegraded: nil,
            stalledWrites: [],
            stalledWriteTotals: FfiStalledWriteTotals(
                unroutable: 1, unsignable: 1, undeliverable: 1, omittedDetails: 1, detailLimit: 1
            )
        )
        assertFullyMirrored(ffi, DiagnosticsSnapshot.init)
    }
}
