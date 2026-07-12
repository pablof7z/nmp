import XCTest

final class NMPUIGalleryUITests: XCTestCase {
    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.tabBars.buttons["Components"].waitForExistence(timeout: 20))
    }

    func testPrimaryCatalogSurfacesAreReachable() {
        XCTAssertTrue(app.staticTexts["Identity primitives"].exists)
        XCTAssertTrue(app.staticTexts["Channel preview"].exists)

        app.tabBars.buttons["Content"].tap()
        XCTAssertTrue(app.staticTexts["Mention variants"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Article primitives"].exists)

        app.tabBars.buttons["Live proof"].tap()
        XCTAssertTrue(app.staticTexts["Only two relay facts enter the app"].waitForExistence(timeout: 5))
        keepScreenshot("live-proof")
    }

    func testConformanceStatesExposeDeterministicFallbacks() {
        app.tabBars.buttons["States"].tap()
        XCTAssertTrue(element("gallery.states.scripted").waitForExistence(timeout: 5))

        let scroll = app.scrollViews.firstMatch
        scroll.swipeUp()
        XCTAssertTrue(element("gallery.states.unknown-kind").waitForExistence(timeout: 5))

        for _ in 0..<5 { scroll.swipeUp() }
        XCTAssertTrue(element("gallery.states.long-content").waitForExistence(timeout: 5))
        keepScreenshot("conformance-long-content")
    }

    func testRapidScrollReachesEndWithoutLosingTheContentView() {
        app.tabBars.buttons["Stress"].tap()
        XCTAssertTrue(app.staticTexts.matching(identifier: "gallery.stress.active-count").firstMatch.waitForExistence(timeout: 5))

        let scroll = app.scrollViews["gallery.stress.scroll"]
        XCTAssertTrue(scroll.exists)
        for _ in 0..<18 { scroll.swipeUp(velocity: .fast) }

        XCTAssertTrue(app.staticTexts["gallery.stress.end"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["End of 72-row stress list"].exists)

        for _ in 0..<18 { scroll.swipeDown(velocity: .fast) }
        let activeCount = app.staticTexts.matching(identifier: "gallery.stress.active-count").firstMatch
        XCTAssertTrue(activeCount.waitForExistence(timeout: 5))
        XCTAssertTrue(
            app.staticTexts["Seventy-two rows. Two live references each."].isHittable,
            "rapid scroll did not return to the top of the stress gallery"
        )
        let bounded = NSPredicate { object, _ in
            guard let element = object as? XCUIElement,
                  let count = Int(element.label.split(separator: " ").first ?? "")
            else { return false }
            return count <= 24
        }
        let result = XCTWaiter.wait(
            for: [XCTNSPredicateExpectation(predicate: bounded, object: activeCount)],
            timeout: 8
        )
        XCTAssertEqual(
            result,
            .completed,
            "visible reference claims did not return to a bounded window; observed: \(activeCount.label)"
        )
        keepScreenshot("stress-returned-to-top")
    }

    private func element(_ identifier: String) -> XCUIElement {
        app.descendants(matching: .any)[identifier]
    }

    private func keepScreenshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
