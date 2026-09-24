// The styled source editor: the view text is the CRDT text, markers and
// all, and page ink is anchored to source lines so it moves with them
// when the layout above changes.

import XCTest

final class StyledEditorUITests: XCTestCase {
    private func launch() -> XCUIApplication {
        let app = XCUIApplication()
        let env = ProcessInfo.processInfo.environment
        app.launchArguments = ["-pairURI", env["KRABINK_TEST_PAIR"] ?? ""]
        app.launch()
        return app
    }

    private func editor(_ app: XCUIApplication) -> XCUIElement {
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        return editor
    }

    private func originY(_ app: XCUIApplication) -> Int? {
        app.staticTexts["sketchStatus"].label.firstMatch(of: /originY=(\d+)/)
            .flatMap { Int($0.output.1) }
    }

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

    /// Poll the status line until `originY` satisfies `pass`.
    private func waitOriginY(
        _ app: XCUIApplication, timeout: TimeInterval, _ pass: (Int) -> Bool
    ) -> Int? {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let y = originY(app), pass(y) { return y }
            usleep(200_000)
        }
        return originY(app)
    }

    /// Typing markdown leaves the source intact in the editor: markers
    /// are styled, never hidden or rewritten.
    func testSourceTextIsPreserved() {
        let app = launch()
        app.buttons["newNote"].tap()
        let editor = editor(app)
        editor.tap()
        let source = "# Title\n\n- item **bold**\n`code`\n"
        editor.typeText(source)
        XCTAssertTrue(app.staticTexts["Title"].waitForExistence(timeout: 5))
        XCTAssertEqual(editor.value as? String, source)
    }

    /// Ink on the second line follows it: turning the first line into a
    /// heading pushes the line (and its ink) down; removing the marker
    /// brings it back.
    func testInkFollowsItsLineWhenHeadingAboveChanges() {
        let app = launch()
        app.buttons["newNote"].tap()
        let editor = editor(app)
        editor.tap()
        editor.typeText("Title\nbody\n")

        // Draw on "body" (second line: below the 16 pt inset and one
        // body line).
        let start = editor.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: 60, dy: 44))
        let end = editor.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: 220, dy: 50))
        start.press(forDuration: 0.1, thenDragTo: end, withVelocity: .slow, thenHoldForDuration: 0.1)
        waitStatus(app, contains: "strokes=1", timeout: 10)
        guard let before = originY(app) else {
            return XCTFail("no originY in \(app.staticTexts["sketchStatus"].label)")
        }
        XCTAssertGreaterThan(before, 16, "ink is not on the second line")

        // Caret to the start of the text, then make line 0 a heading.
        editor.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: 20, dy: 22)).tap()
        editor.typeText("# ")
        XCTAssertTrue((editor.value as? String ?? "").hasPrefix("# Title"), editor.value as? String ?? "")
        let grown = waitOriginY(app, timeout: 5) { $0 > before }
        XCTAssertNotNil(grown)
        XCTAssertGreaterThan(grown ?? before, before, "ink did not move down under the heading")

        // Remove the marker again: the line and its ink come back up.
        editor.typeText(XCUIKeyboardKey.delete.rawValue)
        editor.typeText(XCUIKeyboardKey.delete.rawValue)
        XCTAssertTrue((editor.value as? String ?? "").hasPrefix("Title"), editor.value as? String ?? "")
        let back = waitOriginY(app, timeout: 5) { $0 == before }
        XCTAssertEqual(back, before, "ink did not return with its line")
    }
}
