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
    }

    func testConformanceStatesExposeDeterministicFallbacks() {
        app.tabBars.buttons["States"].tap()
        XCTAssertTrue(element("gallery.states.scripted").waitForExistence(timeout: 5))

        let scroll = app.scrollViews.firstMatch
        scroll.swipeUp()
        XCTAssertTrue(element("gallery.states.unknown-kind").waitForExistence(timeout: 5))

        for _ in 0..<5 { scroll.swipeUp() }
        XCTAssertTrue(element("gallery.states.long-content").waitForExistence(timeout: 5))
    }

    func testRapidScrollReachesEndWithoutLosingTheContentView() {
        app.tabBars.buttons["Stress"].tap()
        XCTAssertTrue(app.staticTexts.matching(identifier: "gallery.stress.active-count").firstMatch.waitForExistence(timeout: 5))

        let scroll = app.scrollViews["gallery.stress.scroll"]
        XCTAssertTrue(scroll.exists)
        for _ in 0..<18 { scroll.swipeUp(velocity: .fast) }

        XCTAssertTrue(app.staticTexts["gallery.stress.end"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["End of 72-row stress list"].exists)
    }

    private func element(_ identifier: String) -> XCUIElement {
        app.descendants(matching: .any)[identifier]
    }
}
