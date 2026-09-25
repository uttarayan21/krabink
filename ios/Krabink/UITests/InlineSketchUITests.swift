// Inline sketches: in the editor the embed is a source line; in the
// reading view it is a box in the text flow the Pencil draws into. The
// simulator runs under `.anyInput`, so a one-finger drag inks. The status
// line reports `inline=` (elements over every box) and `boxY=` (page y of
// the first box) next to the overlay's `strokes=`.

import XCTest

final class InlineSketchUITests: XCTestCase {
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

    private func status(_ app: XCUIApplication) -> String {
        app.staticTexts["sketchStatus"].label
    }

    private func boxY(_ app: XCUIApplication) -> Int? {
        status(app).firstMatch(of: /boxY=(\d+)/).flatMap { Int($0.output.1) }
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

    /// Poll the status line until `boxY` satisfies `pass`.
    @discardableResult
    private func waitBoxY(_ app: XCUIApplication, timeout: TimeInterval, _ pass: (Int) -> Bool) -> Int? {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let y = boxY(app), pass(y) { return y }
            usleep(200_000)
        }
        let y = boxY(app)
        XCTAssertTrue(y.map(pass) ?? false, "boxY never satisfied; at: \(status(app))")
        return y
    }

    /// One-finger drag on the editor between two points in its own
    /// coordinates (content offset zero: page space).
    private func draw(on editor: XCUIElement, from: CGPoint, to: CGPoint) {
        let size = editor.frame.size
        let a = CGVector(dx: from.x / size.width, dy: from.y / size.height)
        let b = CGVector(dx: to.x / size.width, dy: to.y / size.height)
        editor.coordinate(withNormalizedOffset: a)
            .press(
                forDuration: 0.1, thenDragTo: editor.coordinate(withNormalizedOffset: b),
                withVelocity: .slow, thenHoldForDuration: 0.1)
    }

    /// A new note with a heading and one inline sketch under it, switched
    /// to the reading view; returns the editor and the box's page y.
    private func noteWithSketch(_ app: XCUIApplication, heading: String) -> (XCUIElement, CGFloat) {
        app.buttons["newNote"].tap()
        let editor = editor(app)
        editor.tap()
        editor.typeText("# \(heading)\n")
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 5))
        app.buttons["newSketch"].tap()
        // The editor shows the embed as text: no box yet.
        XCTAssertTrue(
            (editor.value as? String ?? "").contains("![sketch](krabink://sketch/"),
            "embed line missing from the source: \(editor.value ?? "")")
        waitBoxY(app, timeout: 5) { $0 == 0 }
        app.buttons["previewToggle"].tap()
        let y = waitBoxY(app, timeout: 5) { $0 > 0 } ?? 0
        return (editor, CGFloat(y))
    }

    /// Inserting a sketch puts an embed line under the caret's line; in
    /// the reading view it is a box: a drag inside it is inline ink, a
    /// drag below it overlay ink; erase-last takes the newest of either.
    func testInsertSketchAndDrawInside() {
        let app = launch()
        let (editor, top) = noteWithSketch(app, heading: "Inline")
        XCTAssertTrue(status(app).contains("inline=0 "), status(app))

        draw(on: editor, from: CGPoint(x: 60, y: top + 50), to: CGPoint(x: 260, y: top + 90))
        waitStatus(app, contains: "inline=1", timeout: 10)
        XCTAssertTrue(status(app).contains("strokes=0 "), "inline stroke counted as overlay: \(status(app))")

        // Well below the box (minimum height 160, plus the box's own spacing).
        draw(on: editor, from: CGPoint(x: 60, y: top + 260), to: CGPoint(x: 260, y: top + 300))
        waitStatus(app, contains: "strokes=1", timeout: 10)
        XCTAssertTrue(status(app).contains("inline=1 "), status(app))

        // The overlay stroke is the newest; then the inline one.
        app.buttons["eraseLast"].tap()
        waitStatus(app, contains: "strokes=0", timeout: 5)
        XCTAssertTrue(status(app).contains("inline=1 "), status(app))
        app.buttons["eraseLast"].tap()
        waitStatus(app, contains: "inline=0", timeout: 5)
    }

    /// The box sits in the text flow: text added above it pushes it down.
    func testBoxFollowsTextAbove() {
        let app = launch()
        let (editor, top) = noteWithSketch(app, heading: "Flow")
        // Back to the editor: caret to the end of the heading line, then a
        // line above the embed.
        app.buttons["previewToggle"].tap()
        waitBoxY(app, timeout: 5) { $0 == 0 }
        editor.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.02)).tap()
        editor.typeText("\nabove the box")
        app.buttons["previewToggle"].tap()
        waitBoxY(app, timeout: 5) { CGFloat($0) > top + 10 }
    }

    /// Inline ink is in the note: it comes back when the note is reopened.
    func testInlineInkSurvivesReopen() {
        let app = launch()
        let sidebar = app.collectionViews.firstMatch
        let (editor, top) = noteWithSketch(app, heading: "Keep inline")
        draw(on: editor, from: CGPoint(x: 60, y: top + 50), to: CGPoint(x: 260, y: top + 90))
        waitStatus(app, contains: "inline=1", timeout: 10)

        app.buttons["newNote"].tap()
        waitStatus(app, contains: "inline=0", timeout: 5)
        sidebar.staticTexts["Keep inline"].firstMatch.tap()
        // A reopened note starts in the editor: the box comes back with
        // the reading view.
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        app.buttons["previewToggle"].tap()
        waitStatus(app, contains: "inline=1", timeout: 10)
        waitBoxY(app, timeout: 5) { $0 > 0 }
    }

    /// Leaving the reading view turns the box back into its source line:
    /// the inline ink goes with it and the embed text is editable again.
    func testEditorShowsEmbedAsText() {
        let app = launch()
        let (editor, top) = noteWithSketch(app, heading: "Edit")
        draw(on: editor, from: CGPoint(x: 60, y: top + 50), to: CGPoint(x: 260, y: top + 90))
        waitStatus(app, contains: "inline=1", timeout: 10)
        XCTAssertFalse(app.buttons["newSketch"].isEnabled)
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "inline-preview"
        shot.lifetime = .keepAlways
        add(shot)

        app.buttons["previewToggle"].tap()
        waitStatus(app, contains: "inline=0", timeout: 5)
        waitBoxY(app, timeout: 5) { $0 == 0 }
        XCTAssertTrue(app.buttons["newSketch"].isEnabled)
        XCTAssertTrue((editor.value as? String ?? "").contains("![sketch](krabink://sketch/"))

        // And back: same box, same ink.
        app.buttons["previewToggle"].tap()
        waitStatus(app, contains: "inline=1", timeout: 10)
        waitBoxY(app, timeout: 5) { $0 > 0 }
    }
}
