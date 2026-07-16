import XCTest

final class NMPUIGalleryUITests: XCTestCase {
    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.tabBars.buttons["Components"].waitForExistence(timeout: 20))
    }

    func testComponentOwnedReferenceCatalogIsReachable() {
        XCTAssertTrue(app.staticTexts["Identity primitives"].exists)
        XCTAssertTrue(app.staticTexts["Channel preview"].exists)

        app.tabBars.buttons["Content"].tap()
        XCTAssertTrue(app.staticTexts["Literal profile · zero fetch"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Literal event · zero fetch"].exists)

        let scroll = app.scrollViews.firstMatch
        for _ in 0..<4 { scroll.swipeUp(velocity: .slow) }
        XCTAssertTrue(app.staticTexts["Replaceable outer loader"].waitForExistence(timeout: 5))
        keepScreenshot("component-owned-content")
    }

    func testLiveProofShowsVisibleDemandDiagnostics() {
        app.tabBars.buttons["Live proof"].tap()
        XCTAssertTrue(
            app.staticTexts["Visible component-owned observations"]
                .waitForExistence(timeout: 5)
        )
        XCTAssertTrue(app.staticTexts["Engine relay diagnostics"].exists)
        keepScreenshot("live-proof")
    }

    func testConnectedFollowButtonsExposeNMPsSignedOutState() {
        let scroll = app.scrollViews.firstMatch
        let featuredUsers = app.staticTexts["Featured users"]
        for _ in 0..<5 where !featuredUsers.isHittable {
            scroll.swipeUp(velocity: .slow)
        }
        XCTAssertTrue(featuredUsers.waitForExistence(timeout: 5))

        let followButtons = app.buttons.matching(NSPredicate(format: "label == 'Follow'"))
        XCTAssertGreaterThanOrEqual(followButtons.count, 3)
        for index in 0..<3 {
            let button = followButtons.element(boundBy: index)
            XCTAssertEqual(button.value as? String, "Signed out")
            XCTAssertFalse(button.isEnabled)
        }
        keepScreenshot("connected-follow-signed-out")
    }

    func testConformanceStatesRetainAccessibilityAndFollowProofs() throws {
        app.tabBars.buttons["States"].tap()
        XCTAssertTrue(element("gallery.states.reference-policies").waitForExistence(timeout: 5))

        let scroll = app.scrollViews.firstMatch
        scroll.swipeUp()
        XCTAssertTrue(element("gallery.states.unknown-kind").waitForExistence(timeout: 5))

        let dynamicTypeTitle = app.staticTexts["Accessibility Dynamic Type"]
        for _ in 0..<5 where !dynamicTypeTitle.isHittable {
            scroll.swipeUp(velocity: .slow)
        }
        XCTAssertTrue(dynamicTypeTitle.waitForExistence(timeout: 5))
        try app.performAccessibilityAudit(for: [.textClipped])

        let followStates = element("gallery.states.follow")
        for _ in 0..<5 where !followStates.isHittable {
            scroll.swipeUp(velocity: .slow)
        }
        XCTAssertTrue(followStates.waitForExistence(timeout: 5))
        let missingList = app.buttons.matching(
            NSPredicate(format: "value == 'No contact list'")
        ).firstMatch
        XCTAssertTrue(missingList.exists)
        XCTAssertFalse(missingList.isEnabled)
        let retry = app.buttons.matching(
            NSPredicate(format: "label == 'Retry follow'")
        ).firstMatch
        XCTAssertEqual(retry.value as? String, "Ready to retry")
        XCTAssertTrue(retry.isEnabled)

        for _ in 0..<7 { scroll.swipeUp() }
        XCTAssertTrue(element("gallery.states.long-content").waitForExistence(timeout: 5))
        keepScreenshot("conformance-long-content")
    }

    func testRapidVisibilityChurnReachesBothEnds() {
        app.tabBars.buttons["Stress"].tap()
        XCTAssertTrue(element("gallery.stress.wire-count").waitForExistence(timeout: 5))

        let scroll = app.scrollViews["gallery.stress.scroll"]
        XCTAssertTrue(scroll.exists)
        for _ in 0..<18 { scroll.swipeUp(velocity: .fast) }

        XCTAssertTrue(element("gallery.stress.end").waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["End of 72-row stress list"].exists)

        for _ in 0..<18 { scroll.swipeDown(velocity: .fast) }
        XCTAssertTrue(
            app.staticTexts["Seventy-two rows. Two independently owned references each."].waitForExistence(timeout: 5),
            "rapid visibility churn did not return to the top"
        )
        keepScreenshot("stress-returned-to-top")
    }

    func testPrimaryCatalogPassesVoiceOverAccessibilityAudit() throws {
        try app.performAccessibilityAudit(
            for: [
                .elementDetection,
                .hitRegion,
                .sufficientElementDescription,
                .textClipped,
                .trait,
            ]
        )
    }

    private func element(_ identifier: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: identifier).firstMatch
    }

    private func keepScreenshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
