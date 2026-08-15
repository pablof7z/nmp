// #1624: the missing-capability identity is a VALUE, and it is the same value
// on both SDKs.
//
// `MissingReplaceableCapabilityTest.kt` is this file's exact mirror: the same
// two 16-byte compiled-capability identifiers, the same expected
// lowercase-hex spellings, the same assertions about equality. Cross-SDK
// agreement is proven by the two suites agreeing on those literals; the
// constants below and in the Kotlin file must be edited together.
//
// Swift's `Data` was already a value type, so the bug this falsifies never
// existed here -- what it holds is the SPELLING. Both surfaces name the
// identity `programHex`/`formatHex` and render the same bytes the same way,
// so an app that ports its handling from one SDK to the other cannot find a
// different shape waiting.

import Foundation
import XCTest
@testable import NMP
import NMPFFI

final class MissingReplaceableCapabilityTests: XCTestCase {
    func testFfiBytesProjectAsCanonicalLowercaseHex() {
        let error = NMPError(.MissingReplaceableCapability(
            program: Data(programBytes),
            format: Data(formatBytes)
        ))
        guard case .missingReplaceableCapability(let program, let format) = error else {
            return XCTFail("the FFI refusal must project as .missingReplaceableCapability")
        }

        XCTAssertEqual(program, programHex)
        XCTAssertEqual(format, formatHex)
        XCTAssertEqual(program.count, 32)
        XCTAssertEqual(format.count, 32)
    }

    func testTwoErrorsNamingTheSameCapabilityAreOneValue() {
        let first = NMPError(.MissingReplaceableCapability(
            program: Data(programBytes),
            format: Data(formatBytes)
        ))
        let second = NMPError(.MissingReplaceableCapability(
            program: Data(programBytes),
            format: Data(formatBytes)
        ))

        XCTAssertEqual(first, second)
        XCTAssertEqual(first, .missingReplaceableCapability(
            programHex: programHex,
            formatHex: formatHex
        ))
    }

    func testDistinctCapabilitiesStayDistinct() {
        XCTAssertNotEqual(
            NMPError.missingReplaceableCapability(programHex: programHex, formatHex: formatHex),
            NMPError.missingReplaceableCapability(programHex: formatHex, formatHex: programHex)
        )
    }
}

// Exercises a leading zero byte, the 0x0f/0x10 nibble boundary, the sign
// boundary at 0x7f/0x80, and 0xff -- every way a naive hex encoder goes wrong.
private let programBytes: [UInt8] = [
    0x00, 0x01, 0x0f, 0x10,
    0x7f, 0x80, 0xfe, 0xff,
    0x2a, 0x3b, 0x4c, 0x5d,
    0x6e, 0x7f, 0x80, 0x91,
]
private let programHex = "00010f107f80feff2a3b4c5d6e7f8091"

private let formatBytes: [UInt8] = [
    0xff, 0xee, 0xdd, 0xcc,
    0xbb, 0xaa, 0x99, 0x88,
    0x77, 0x66, 0x55, 0x44,
    0x33, 0x22, 0x11, 0x00,
]
private let formatHex = "ffeeddccbbaa99887766554433221100"
