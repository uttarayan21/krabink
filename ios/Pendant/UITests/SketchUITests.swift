// iM3 e2e driver, same phase pattern as SyncUITests:
//   phase1: create note, create sketch, draw one stroke      (sim → relay)
//   [harness: probe --expect-strokes 1 --add-stroke]
//   phase2: reopen sketch, see the probe's stroke (strokes=2),
//           erase the last stroke                            (relay → sim → relay)
//   [harness: probe --expect-strokes 1]

import XCTest

final class SketchUITests: XCTestCase {
    private func launch(extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        let env = ProcessInfo.processInfo.environment
        app.launchArguments = [
            "-serverURL", env["PENDANT_TEST_SERVER"] ?? "ws://127.0.0.1:8722/ws",
            "-token", env["PENDANT_TEST_TOKEN"] ?? "demo",
        ] + extra
        app.launch()
        return app
    }

    /// Relative luminance (0…1) of the screen pixel at `point` (screen
    /// points) in a fresh screenshot.
    private func luminance(at point: CGPoint) -> Double {
        let image = XCUIScreen.main.screenshot().image
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let one = UIGraphicsImageRenderer(size: CGSize(width: 1, height: 1), format: format).image { _ in
            image.draw(at: CGPoint(x: -point.x, y: -point.y))
        }
        guard let cg = one.cgImage, let data = cg.dataProvider?.data, let bytes = CFDataGetBytePtr(data)
        else { return -1 }
        let alphaFirst = cg.alphaInfo == .premultipliedFirst || cg.alphaInfo == .first
        let littleEndian = cg.bitmapInfo.contains(.byteOrder32Little)
        let (r, g, b): (UInt8, UInt8, UInt8)
        if alphaFirst, littleEndian {
            (r, g, b) = (bytes[2], bytes[1], bytes[0])
        } else if alphaFirst {
            (r, g, b) = (bytes[1], bytes[2], bytes[3])
        } else {
            (r, g, b) = (bytes[0], bytes[1], bytes[2])
        }
        return (0.2126 * Double(r) + 0.7152 * Double(g) + 0.0722 * Double(b)) / 255
    }

    private func waitConnected(_ app: XCUIApplication) {
        let state = app.staticTexts["syncState"]
        expectation(
            for: NSPredicate(format: "label BEGINSWITH 'connected'"), evaluatedWith: state)
        waitForExpectations(timeout: 15)
    }

    private func waitStatus(_ app: XCUIApplication, contains text: String, timeout: TimeInterval) {
        let status = app.staticTexts["sketchStatus"]
        let result = XCTWaiter().wait(
            for: [
                expectation(
                    for: NSPredicate(format: "label CONTAINS %@", text), evaluatedWith: status)
            ],
            timeout: timeout)
        XCTAssertEqual(result, .completed, "sketch status never showed \(text); at: \(status.label)")
    }

    func testPhase1CreateSketchAndDraw() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        app.buttons["sketchMenu"].tap()
        app.buttons["newSketch"].tap()

        let canvas = app.descendants(matching: .any)["sketchCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 5))
        let start = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.3))
        let end = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.7, dy: 0.55))
        start.press(forDuration: 0.1, thenDragTo: end, withVelocity: .slow, thenHoldForDuration: 0.1)

        waitStatus(app, contains: "strokes=1", timeout: 10)
        // iM4: the drag must have opened a wet stream and flushed ≥1 batch.
        waitStatus(app, contains: "wetSent=", timeout: 2)
        let label = app.staticTexts["sketchStatus"].label
        let sent = label.firstMatch(of: /wetSent=(\d+)/).map { Int($0.output.1) ?? 0 } ?? 0
        XCTAssertGreaterThanOrEqual(sent, 1, "no wet batch streamed during draw: \(label)")
        // Let the Flush::Immediate commit reach the relay before teardown.
        sleep(2)
    }

    func testPhase2SeesRemoteStrokeAndErases() {
        let app = launch()
        waitConnected(app)

        let row = app.cells.firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.tap()
        app.buttons["sketchMenu"].tap()
        app.buttons["sketch-0"].tap()

        // Probe added a second stroke while the app was closed; catch-up
        // must land it on the open canvas.
        waitStatus(app, contains: "strokes=2", timeout: 15)

        // Vector erase: drop the last stroke (the probe's) and let the
        // removal sync back.
        app.buttons["eraseLast"].tap()
        waitStatus(app, contains: "strokes=1", timeout: 10)
        sleep(2)
    }

    // Draw-and-hold: a drag that ends with the pen held still snaps to a
    // shape (a straight drag is a line) and commits a shape element.
    /// The marker is write-once ink: where a stroke crosses itself it is no
    /// darker than along an arm. `-figureEight 1` commits a lemniscate
    /// (XCUITest drags are straight lines) crossing at canvas (300, 300),
    /// arm tip at (440, 300).
    func testMarkerSelfOverlapDoesNotDarken() {
        let app = launch(extra: ["-tool", "marker", "-figureEight", "1"])
        waitConnected(app)

        app.buttons["newNote"].tap()
        app.buttons["sketchMenu"].tap()
        app.buttons["newSketch"].tap()

        let canvas = app.descendants(matching: .any)["sketchCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 5))
        waitStatus(app, contains: "strokes=1", timeout: 10)
        sleep(1)
        let origin = canvas.frame.origin
        let crossing = luminance(at: CGPoint(x: origin.x + 300, y: origin.y + 300))
        let arm = luminance(at: CGPoint(x: origin.x + 440, y: origin.y + 300))
        let paper = luminance(at: CGPoint(x: origin.x + 300, y: origin.y + 520))
        XCTAssertLessThan(arm, paper - 0.08, "no marker ink on the arm: arm=\(arm) paper=\(paper)")
        XCTAssertEqual(crossing, arm, accuracy: 0.05, "self-crossing darkened: crossing=\(crossing) arm=\(arm)")
        sleep(1)
    }

    func testHoldSnapsToShape() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        app.buttons["sketchMenu"].tap()
        app.buttons["newSketch"].tap()

        let canvas = app.descendants(matching: .any)["sketchCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 5))
        let start = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.2, dy: 0.3))
        let end = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.7, dy: 0.5))
        start.press(forDuration: 0.1, thenDragTo: end, withVelocity: .slow, thenHoldForDuration: 0.9)

        waitStatus(app, contains: "shapes=1", timeout: 10)
        XCTAssertTrue(app.staticTexts["sketchStatus"].label.contains("strokes=0"))

        // The shape is an element like any other: erase drops it.
        app.buttons["eraseLast"].tap()
        waitStatus(app, contains: "shapes=0", timeout: 10)
        sleep(1)
    }

    // iM5: creating a sketch splices an inline embed; preview renders it as a
    // tappable thumbnail that reopens the canvas.
    func testInlineEmbedPreviewTapOpens() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        app.buttons["sketchMenu"].tap()
        app.buttons["newSketch"].tap()

        let canvas = app.descendants(matching: .any)["sketchCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 5))
        canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.3, dy: 0.35))
            .press(
                forDuration: 0.1,
                thenDragTo: canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.6, dy: 0.5)),
                withVelocity: .slow, thenHoldForDuration: 0.1)
        waitStatus(app, contains: "strokes=1", timeout: 10)
        app.buttons["sketchDone"].tap()

        // Editor holds the spliced embed markdown.
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        let editorText = editor.value as? String ?? ""
        XCTAssertTrue(
            editorText.contains("pendant://sketch/"),
            "embed not spliced into note text: \(editorText)")

        // Preview renders the embed; tapping the thumbnail reopens the canvas.
        app.buttons["previewToggle"].tap()
        let preview = app.textViews["preview"]
        XCTAssertTrue(preview.waitForExistence(timeout: 5))
        // Embed sits at the top of an otherwise-empty note; tap the thumbnail.
        preview.coordinate(withNormalizedOffset: CGVector(dx: 0.12, dy: 0.08)).tap()
        XCTAssertTrue(
            canvas.waitForExistence(timeout: 5), "tapping the inline embed did not open the canvas")
    }

    // Regression: the cached SketchModel reattaches to a fresh canvas on every
    // open; strokes must repaint (and must NOT be diffed away as erased).
    func testReopenKeepsStrokes() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        app.buttons["sketchMenu"].tap()
        app.buttons["newSketch"].tap()

        let canvas = app.descendants(matching: .any)["sketchCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 5))
        canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.3, dy: 0.35))
            .press(
                forDuration: 0.1,
                thenDragTo: canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.6, dy: 0.5)),
                withVelocity: .slow, thenHoldForDuration: 0.1)
        waitStatus(app, contains: "strokes=1", timeout: 10)
        app.buttons["sketchDone"].tap()

        // Second open (cached model, new canvas): the stroke must repaint.
        app.buttons["sketchMenu"].tap()
        app.buttons["sketch-0"].tap()
        waitStatus(app, contains: "strokes=1", timeout: 10)

        // Draw on the reopened canvas: adds, never wipes the old stroke.
        canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.3, dy: 0.6))
            .press(
                forDuration: 0.1,
                thenDragTo: canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.6, dy: 0.7)),
                withVelocity: .slow, thenHoldForDuration: 0.1)
        waitStatus(app, contains: "strokes=2", timeout: 10)
        app.buttons["sketchDone"].tap()

        // Third open: both strokes still there.
        app.buttons["sketchMenu"].tap()
        app.buttons["sketch-0"].tap()
        waitStatus(app, contains: "strokes=2", timeout: 10)
    }

    // The sidebar row mirrors the first markdown heading of the note.
    func testSidebarTitleFollowsHeading() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        editor.typeText("# Groceries\nmilk\n")

        let row = app.staticTexts["Groceries"]
        XCTAssertTrue(
            row.waitForExistence(timeout: 5), "sidebar never showed the heading-derived title")
    }

    // Swipe-to-delete drops the note from the registry (and the sidebar).
    func testDeleteNoteRemovesFromSidebar() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        editor.typeText("# DeleteMe\n")

        let row = app.staticTexts["DeleteMe"]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.swipeLeft()
        let delete = app.buttons["deleteNote"]
        XCTAssertTrue(delete.waitForExistence(timeout: 3))
        delete.tap()
        expectation(for: NSPredicate(format: "exists == false"), evaluatedWith: row)
        waitForExpectations(timeout: 5)
    }

    // Bulk delete: select mode picks several notes; deletion needs TWO
    // confirmations, and cancelling the second one deletes nothing.
    func testBulkDeleteDoubleConfirm() {
        let app = launch()
        waitConnected(app)

        for name in ["BulkA", "BulkB"] {
            app.buttons["newNote"].tap()
            let editor = app.textViews["editor"]
            XCTAssertTrue(editor.waitForExistence(timeout: 5))
            editor.tap()
            editor.typeText("# \(name)\n")
            XCTAssertTrue(app.staticTexts[name].waitForExistence(timeout: 5))
        }

        app.buttons["selectNotes"].tap()
        // "BulkA" can match both the sidebar row and detail-pane content;
        // scope taps to the sidebar list.
        let sidebar = app.collectionViews.firstMatch
        sidebar.staticTexts["BulkA"].firstMatch.tap()
        sidebar.staticTexts["BulkB"].firstMatch.tap()

        // Back out at the second confirmation: nothing may be deleted.
        app.buttons["bulkDelete"].tap()
        XCTAssertTrue(app.alerts.buttons["delete"].waitForExistence(timeout: 3))
        app.alerts.buttons["delete"].tap()
        XCTAssertTrue(app.alerts.buttons["delete forever"].waitForExistence(timeout: 3))
        app.alerts.buttons["cancel"].tap()
        XCTAssertTrue(
            sidebar.staticTexts["BulkA"].exists, "cancel at second confirm deleted notes")
        XCTAssertTrue(
            sidebar.staticTexts["BulkB"].exists, "cancel at second confirm deleted notes")

        // Confirm both prompts: both notes gone from the sidebar.
        app.buttons["bulkDelete"].tap()
        XCTAssertTrue(app.alerts.buttons["delete"].waitForExistence(timeout: 3))
        app.alerts.buttons["delete"].tap()
        XCTAssertTrue(app.alerts.buttons["delete forever"].waitForExistence(timeout: 3))
        app.alerts.buttons["delete forever"].tap()
        expectation(
            for: NSPredicate(format: "exists == false"),
            evaluatedWith: sidebar.staticTexts["BulkA"])
        expectation(
            for: NSPredicate(format: "exists == false"),
            evaluatedWith: sidebar.staticTexts["BulkB"])
        waitForExpectations(timeout: 10)
    }
}
