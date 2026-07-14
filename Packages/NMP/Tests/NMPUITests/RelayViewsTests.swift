import NMP
@testable import NMPUI
import SwiftUI
import XCTest

final class RelayViewsTests: XCTestCase {
    func testRelayPresentationTrimsAdvertisedFieldsAndFallsBackDeterministically() {
        let advertised = NMPRelayPresentation(
            relay: "wss://relay.example",
            advertisedName: "  Example Relay  ",
            advertisedDescription: "  A useful relay  ",
            advertisedIcon: "https://media.example/icon.png",
            freshness: .fresh
        )
        XCTAssertEqual(advertised.displayName, "Example Relay")
        XCTAssertEqual(advertised.displayDescription, "A useful relay")
        XCTAssertEqual(advertised.initials, "ER")

        let fallback = NMPRelayPresentation(
            relay: "wss://fallback.example:443/path",
            advertisedName: "  ",
            advertisedDescription: nil,
            freshness: .fresh
        )
        XCTAssertEqual(fallback.displayName, "fallback.example")
        XCTAssertEqual(fallback.displayDescription, "No relay description provided")
        XCTAssertEqual(fallback.initials, "FA")
    }

    func testFreshStaleLastGoodAndUnavailableRemainDistinct() {
        let fresh = NMPRelayInformationState.available(
            NMPRelayPresentation(
                relay: "wss://fresh.example",
                advertisedName: "Fresh",
                freshness: .fresh
            )
        )
        let stale = NMPRelayInformationState.available(
            NMPRelayPresentation(
                relay: "wss://stale.example",
                advertisedName: "Last good name",
                advertisedDescription: "Last good description",
                freshness: .stale,
                lastError: "refresh timed out"
            )
        )
        let unavailable = NMPRelayInformationState.unavailable(
            relay: "wss://missing.example",
            reason: "document unavailable"
        )

        XCTAssertEqual(fresh.informationLabel, "Fresh")
        XCTAssertNil(fresh.lastError)
        XCTAssertEqual(stale.informationLabel, "Stale")
        XCTAssertEqual(stale.displayName, "Last good name")
        XCTAssertEqual(stale.displayDescription, "Last good description")
        XCTAssertEqual(stale.lastError, "refresh timed out")
        XCTAssertEqual(unavailable.informationLabel, "Unavailable")
        XCTAssertEqual(unavailable.displayName, "missing.example")
        XCTAssertEqual(unavailable.displayDescription, "document unavailable")
    }

    func testLoadingAndUnavailableDefaultFallbacksAreExplicit() {
        let loading = NMPRelayInformationState.loading(relay: "wss://loading.example")
        let unavailableWithoutReason = NMPRelayInformationState.unavailable(
            relay: "wss://missing.example",
            reason: nil
        )
        let unavailableWithBlankReason = NMPRelayInformationState.unavailable(
            relay: "wss://blank.example",
            reason: "  \n "
        )

        XCTAssertEqual(loading.displayName, "loading.example")
        XCTAssertEqual(loading.displayDescription, "Relay information is loading")
        XCTAssertEqual(loading.informationLabel, "Loading")
        XCTAssertEqual(loading.initials, "LO")
        XCTAssertNil(loading.presentation)
        XCTAssertNil(loading.lastError)

        for (state, name) in [
            (unavailableWithoutReason, "missing.example"),
            (unavailableWithBlankReason, "blank.example"),
        ] {
            XCTAssertEqual(state.displayName, name)
            XCTAssertEqual(state.displayDescription, "Relay information unavailable")
            XCTAssertEqual(state.informationLabel, "Unavailable")
            XCTAssertNil(state.presentation)
            XCTAssertNil(state.lastError)
        }
    }

    func testEveryProjectedSourceStatusMapsWithoutInventingGlobalHealth() {
        let cases: [(SourceStatus?, NMPRelayRuntimePresentation, String)] = [
            (nil, .statusUnavailable, "Status unavailable"),
            (.requesting, .requesting, "Requesting"),
            (.connecting, .connecting, "Connecting"),
            (.disconnected, .disconnected, "Disconnected"),
            (
                .awaitingAuth(phase: .awaitingPolicy),
                .awaitingAuth(phase: .awaitingPolicy),
                "Awaiting authentication policy"
            ),
            (
                .awaitingAuth(phase: .awaitingSignature),
                .awaitingAuth(phase: .awaitingSignature),
                "Awaiting authentication signature"
            ),
            (.authDenied, .authDenied, "Authentication denied"),
            (.error, .error, "Connection error"),
        ]

        for (source, expected, label) in cases {
            let projected = NMPRelayRuntimePresentation(source)
            XCTAssertEqual(projected, expected)
            XCTAssertEqual(projected.label, label)
        }
    }

    func testEveryControlledSwiftUIPrimitiveConstructsWithoutAnEngineOrLoader() {
        let information = NMPRelayInformationState.available(
            NMPRelayPresentation(
                relay: "wss://relay.example",
                advertisedName: "Relay",
                advertisedDescription: "Description",
                advertisedIcon: "https://media.example/icon.png",
                freshness: .stale,
                lastError: "refresh failed"
            )
        )
        let image = Image(systemName: "antenna.radiowaves.left.and.right")

        let icon = NMPRelayIcon(state: information, image: image)
        _ = NMPRelayName(state: information)
        _ = NMPRelayDescription(state: information)
        _ = NMPRelayRuntimeStatus(status: .connecting)
        let entry = NMPRelayListEntry(
            information: information,
            runtime: .connecting,
            image: image,
            action: {}
        )

        XCTAssertEqual(icon.state, information)
        XCTAssertNotNil(icon.image)
        XCTAssertEqual(entry.information, information)
        XCTAssertNotNil(entry.image)
        XCTAssertEqual(
            entry.accessibilityLabel,
            "Relay Relay. Description. Relay information Stale. Connecting. "
                + "Relay information error: refresh failed"
        )
    }

    func testRelayViewsSourceHasNoAcquisitionOrLoadingOwner() throws {
        let packageRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let source = try String(
            contentsOf: packageRoot.appendingPathComponent("Sources/NMPUI/RelayViews.swift"),
            encoding: .utf8
        )
        for forbidden in [
            "NMPEngine", "relayInformation(", "AsyncImage", "NMPImageLoader",
            "nmpImageLoader", "URLSession", "Timer", "Task {",
        ] {
            XCTAssertFalse(source.contains(forbidden), "Relay views must not contain \(forbidden)")
        }
    }
}
