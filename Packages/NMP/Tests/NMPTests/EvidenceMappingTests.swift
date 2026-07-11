import XCTest
@testable import NMP
import NMPFFI

final class EvidenceMappingTests: XCTestCase {
    func testEveryAcquisitionEvidenceVariantMapsWithoutARollup() {
        let raw = FfiAcquisitionEvidence(
            sources: [
                .init(relay: "wss://requesting.example", reconciledThrough: 10, status: .requesting),
                .init(relay: "wss://connecting.example", reconciledThrough: nil, status: .connecting),
                .init(relay: "wss://disconnected.example", reconciledThrough: 20, status: .disconnected),
                .init(
                    relay: "wss://policy.example",
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingPolicy)
                ),
                .init(
                    relay: "wss://signature.example",
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingSignature)
                ),
                .init(relay: "wss://denied.example", reconciledThrough: nil, status: .authDenied),
                .init(relay: "wss://error.example", reconciledThrough: nil, status: .error),
            ],
            shortfall: [
                .noPlannedSource(atom: "no-source-filter"),
                .noResolvedDemand,
                .localLimit(atom: "limited-filter"),
            ]
        )

        let evidence = AcquisitionEvidence(raw)
        XCTAssertEqual(evidence.sources.map(\.relay), raw.sources.map(\.relay))
        XCTAssertEqual(evidence.sources[0].status, .requesting)
        XCTAssertEqual(evidence.sources[0].reconciledThrough, 10)
        XCTAssertEqual(evidence.sources[1].status, .connecting)
        XCTAssertNil(evidence.sources[1].reconciledThrough)
        XCTAssertEqual(evidence.sources[2].status, .disconnected)
        XCTAssertEqual(evidence.sources[3].status, .awaitingAuth(phase: .awaitingPolicy))
        XCTAssertEqual(evidence.sources[4].status, .awaitingAuth(phase: .awaitingSignature))
        XCTAssertEqual(evidence.sources[5].status, .authDenied)
        XCTAssertEqual(evidence.sources[6].status, .error)
        XCTAssertEqual(
            evidence.shortfall,
            [
                .noPlannedSource(atom: "no-source-filter"),
                .noResolvedDemand,
                .localLimit(atom: "limited-filter"),
            ]
        )
    }

    func testDiagnosticsIntervalIsDistinctFromQueryEvidence() {
        let interval = CoverageInterval(FfiCoverageInterval(from: 4, through: 9))
        XCTAssertEqual(interval.from, 4)
        XCTAssertEqual(interval.through, 9)

        let proven = FilterCoverage(
            FfiFilterCoverage(
                filter: "{\"kinds\":[9999]}",
                coverage: FfiCoverageInterval(from: 4, through: 9)
            )
        )
        XCTAssertEqual(proven.coverage, interval)

        let unproven = FilterCoverage(
            FfiFilterCoverage(filter: "{\"kinds\":[9998]}", coverage: nil)
        )
        XCTAssertNil(unproven.coverage)

        let evidence = AcquisitionEvidence(
            FfiAcquisitionEvidence(
                sources: [
                    .init(
                        relay: "wss://source.example",
                        reconciledThrough: 9,
                        status: .disconnected
                    )
                ],
                shortfall: []
            )
        )
        XCTAssertEqual(evidence.sources[0].reconciledThrough, interval.through)
        XCTAssertEqual(evidence.sources[0].status, .disconnected)
    }
}
