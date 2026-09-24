// Page-ink e2e driver, same phase pattern as SyncUITests. The simulator
// runs under `.anyInput`, so a one-finger drag over the editor inks:
//   phase1: create note, draw one stroke on the page          (sim → relay)
//   [harness: probe --expect-page-elements 1 --add-page-stroke 0]
//   phase2: reopen the note, see the probe's stroke (strokes=2),
//           erase the last stroke                             (relay → sim → relay)
//   [harness: probe --expect-page-elements 1]

import XCTest

final class SketchUITests: XCTestCase {
    private func launch(extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        let env = ProcessInfo.processInfo.environment
        app.launchArguments = [
            "-pairURI", env["KRABINK_TEST_PAIR"] ?? "",
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
        XCTAssertEqual(result, .completed, "ink status never showed \(text); at: \(status.label)")
    }

    /// The note page: the editor text view, which owns the pen gesture.
    private func page(_ app: XCUIApplication) -> XCUIElement {
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        return editor
    }

    /// One-finger drag across the page from `from` to `to` (normalised).
    private func draw(
        on page: XCUIElement, from: CGVector, to: CGVector, hold: TimeInterval = 0.1
    ) {
        page.coordinate(withNormalizedOffset: from)
            .press(
                forDuration: 0.1,
                thenDragTo: page.coordinate(withNormalizedOffset: to),
                withVelocity: .slow, thenHoldForDuration: hold)
    }

    /// A new note titled by `heading`, so the sidebar row can be found again.
    private func newNote(_ app: XCUIApplication, heading: String) -> XCUIElement {
        app.buttons["newNote"].tap()
        let editor = page(app)
        editor.tap()
        editor.typeText("# \(heading)\n")
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 5))
        return editor
    }

    /// The brush lab's corpus page replays a bundled recording through
    /// every input model the core was built with and prints their metrics.
    func testBrushLabReplaysCorpus() {
        let app = launch(extra: ["-brushLab", "1", "-labPage", "2"])
        let metrics = app.staticTexts["labMetrics"]
        XCTAssertTrue(metrics.waitForExistence(timeout: 10))
        let text = metrics.label
        XCTAssertTrue(text.contains("EMA points="), text)
        XCTAssertTrue(text.contains("lag="), text)
        // The iOS core is built with the `ism` feature.
        XCTAssertTrue(text.contains("ISM points="), text)
    }

    func testPhase1CreateNoteAndDraw() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = page(app)
        draw(on: editor, from: CGVector(dx: 0.25, dy: 0.3), to: CGVector(dx: 0.7, dy: 0.55))

        waitStatus(app, contains: "strokes=1", timeout: 10)
        // The drag must have opened a wet stream and flushed ≥1 batch.
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
        _ = page(app)

        // Probe added a second page stroke while the app was closed;
        // catch-up must land it on the open page.
        waitStatus(app, contains: "strokes=2", timeout: 15)

        // Vector erase: drop the last stroke (the probe's) and let the
        // removal sync back.
        app.buttons["eraseLast"].tap()
        waitStatus(app, contains: "strokes=1", timeout: 10)
        sleep(2)
    }

    /// The marker is write-once ink: where a stroke crosses itself it is no
    /// darker than along an arm. `-figureEight 1` commits a lemniscate
    /// (XCUITest drags are straight lines) crossing at page (300, 300),
    /// arm tip at (440, 300); the page starts at the editor's origin.
    func testMarkerSelfOverlapDoesNotDarken() {
        let app = launch(extra: ["-tool", "marker", "-figureEight", "1"])
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = page(app)
        waitStatus(app, contains: "strokes=1", timeout: 10)
        sleep(1)
        let origin = editor.frame.origin
        let crossing = luminance(at: CGPoint(x: origin.x + 300, y: origin.y + 300))
        let arm = luminance(at: CGPoint(x: origin.x + 440, y: origin.y + 300))
        let paper = luminance(at: CGPoint(x: origin.x + 300, y: origin.y + 520))
        XCTAssertLessThan(arm, paper - 0.08, "no marker ink on the arm: arm=\(arm) paper=\(paper)")
        XCTAssertEqual(crossing, arm, accuracy: 0.05, "self-crossing darkened: crossing=\(crossing) arm=\(arm)")
        sleep(1)
    }

    /// `-fakeEstimates 1`: every finger sample claims a pending estimate
    /// that the model revises 60 ms after pen-up, so the commit takes the
    /// settling path. The stroke must land exactly once, with its
    /// revisions counted, and the next stroke must land after it.
    func testEstimateUpdateDoesNotDuplicateStroke() {
        let app = launch(extra: ["-fakeEstimates", "1"])
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = page(app)
        draw(on: editor, from: CGVector(dx: 0.2, dy: 0.3), to: CGVector(dx: 0.6, dy: 0.5))

        waitStatus(app, contains: "strokes=1 ", timeout: 10)
        let status = app.staticTexts["sketchStatus"]
        let revised = status.label.firstMatch(of: /est=(\d+)/).map { Int($0.output.1) ?? 0 } ?? 0
        XCTAssertGreaterThan(revised, 0, "no estimate was revised before commit: \(status.label)")
        sleep(1)
        XCTAssertTrue(status.label.contains("strokes=1 "), "stroke duplicated: \(status.label)")

        draw(on: editor, from: CGVector(dx: 0.2, dy: 0.6), to: CGVector(dx: 0.6, dy: 0.8))
        waitStatus(app, contains: "strokes=2 ", timeout: 10)
    }

    /// A stroke drawn with a bundled custom brush (the crayon) commits with
    /// its spec attached and survives leaving and reopening the note.
    func testCustomBrushStrokeRoundTrips() {
        let app = launch(extra: ["-tool", "crayon"])
        waitConnected(app)

        let editor = newNote(app, heading: "Crayon")
        draw(on: editor, from: CGVector(dx: 0.2, dy: 0.3), to: CGVector(dx: 0.6, dy: 0.5))

        waitStatus(app, contains: "strokes=1 ", timeout: 10)
        waitStatus(app, contains: "custom=1", timeout: 5)

        // Leave for another note and come back: the page is re-read from
        // the CRDT into a fresh canvas.
        app.buttons["newNote"].tap()
        waitStatus(app, contains: "custom=0", timeout: 5)
        app.collectionViews.firstMatch.staticTexts["Crayon"].firstMatch.tap()
        waitStatus(app, contains: "custom=1", timeout: 10)
    }

    // Draw-and-hold: a drag that ends with the pen held still snaps to a
    // shape (a straight drag is a line) and commits a shape element.
    func testHoldSnapsToShape() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = page(app)
        draw(on: editor, from: CGVector(dx: 0.2, dy: 0.3), to: CGVector(dx: 0.7, dy: 0.5), hold: 0.9)

        waitStatus(app, contains: "shapes=1", timeout: 10)
        XCTAssertTrue(app.staticTexts["sketchStatus"].label.contains("strokes=0"))

        // The shape is an element like any other: erase drops it.
        app.buttons["eraseLast"].tap()
        waitStatus(app, contains: "shapes=0", timeout: 10)
        sleep(1)
    }

    // Regression: the cached ink model reattaches to a fresh canvas on every
    // open; strokes must repaint (and must NOT be diffed away as erased).
    func testReopenKeepsStrokes() {
        let app = launch()
        waitConnected(app)

        let sidebar = app.collectionViews.firstMatch
        let editor = newNote(app, heading: "Keep")
        draw(on: editor, from: CGVector(dx: 0.3, dy: 0.35), to: CGVector(dx: 0.6, dy: 0.5))
        waitStatus(app, contains: "strokes=1", timeout: 10)

        // Second open (cached model, new canvas): the stroke must repaint.
        app.buttons["newNote"].tap()
        waitStatus(app, contains: "strokes=0", timeout: 5)
        sidebar.staticTexts["Keep"].firstMatch.tap()
        waitStatus(app, contains: "strokes=1", timeout: 10)

        // Draw on the reopened page: adds, never wipes the old stroke.
        draw(on: editor, from: CGVector(dx: 0.3, dy: 0.6), to: CGVector(dx: 0.6, dy: 0.7))
        waitStatus(app, contains: "strokes=2", timeout: 10)

        // Third open: both strokes still there.
        app.buttons["newNote"].tap()
        waitStatus(app, contains: "strokes=0", timeout: 5)
        sidebar.staticTexts["Keep"].firstMatch.tap()
        waitStatus(app, contains: "strokes=2", timeout: 10)
    }

    // The sidebar row mirrors the first markdown heading of the note.
    func testSidebarTitleFollowsHeading() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = page(app)
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
        let editor = page(app)
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
            let editor = page(app)
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
