// iPhone layout and draw mode, no relay needed. Run on an iPhone
// simulator; on an iPad the test skips (no draw mode there).
//   - the split view collapses: a new note pushes the page, back returns
//     to the list
//   - draw mode off: a one-finger drag scrolls, no ink
//   - draw mode on: the same drag is one stroke

import XCTest

final class PhoneUITests: XCTestCase {
    private func waitStatus(_ app: XCUIApplication, contains text: String, timeout: TimeInterval) {
        let status = app.staticTexts["sketchStatus"]
        let result = XCTWaiter().wait(
            for: [
                expectation(
                    for: NSPredicate(format: "label CONTAINS %@", text), evaluatedWith: status)
            ],
            timeout: timeout)
        XCTAssertEqual(result, .completed, "ink status never showed \(text); at: \(status.label)")
    }

    private func drag(on page: XCUIElement) {
        page.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.3))
            .press(
                forDuration: 0.1,
                thenDragTo: page.coordinate(withNormalizedOffset: CGVector(dx: 0.7, dy: 0.5)),
                withVelocity: .slow, thenHoldForDuration: 0.1)
    }

    private func attach(_ name: String) {
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }

    func testDrawModeGatesFingerInk() throws {
        try XCTSkipUnless(UIDevice.current.userInterfaceIdiom == .phone, "iPhone only")
        let app = XCUIApplication()
        app.launch()

        app.buttons["newNote"].tap()
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        editor.typeText("# Phone\n")
        attach("editor")

        // Draw mode off: the finger is not a pen.
        drag(on: editor)
        sleep(1)
        XCTAssertFalse(
            app.staticTexts["sketchStatus"].label.contains("strokes=1"),
            "a drag inked outside draw mode")

        app.buttons["drawToggle"].tap()
        drag(on: editor)
        waitStatus(app, contains: "strokes=1", timeout: 10)
        attach("drawing")

        app.buttons["drawToggle"].tap()
        drag(on: editor)
        sleep(1)
        XCTAssertTrue(app.staticTexts["sketchStatus"].label.contains("strokes=1"))

        // Back to the list: the note is there under its title.
        app.navigationBars.buttons.firstMatch.tap()
        XCTAssertTrue(app.staticTexts["Phone"].waitForExistence(timeout: 5))
        attach("list")
    }
}
