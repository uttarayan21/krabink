// iM1 e2e driver. Split into phases so the harness can interleave the Rust
// probe peer between launches:
//   phase1: create a note, type into the editor            (sim → relay)
//   [harness runs probe --expect … --append " +rust"]
//   phase2: reopen the note, assert the probe's edit shows (relay → sim)
//
// Server/token come from PENDANT_TEST_SERVER / PENDANT_TEST_TOKEN env vars
// passed through `xcodebuild test`.

import XCTest

final class SyncUITests: XCTestCase {
    private func launch() -> XCUIApplication {
        let app = XCUIApplication()
        let env = ProcessInfo.processInfo.environment
        app.launchArguments = [
            "-serverURL", env["PENDANT_TEST_SERVER"] ?? "ws://127.0.0.1:8722/ws",
            "-token", env["PENDANT_TEST_TOKEN"] ?? "demo",
        ]
        app.launch()
        return app
    }

    private func waitConnected(_ app: XCUIApplication) {
        let state = app.staticTexts["syncState"]
        expectation(
            for: NSPredicate(format: "label BEGINSWITH 'connected'"), evaluatedWith: state)
        waitForExpectations(timeout: 15)
    }

    func testPhase1CreateAndType() {
        let app = launch()
        waitConnected(app)

        app.buttons["newNote"].tap()
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        editor.typeText("# hello from iPad sim")
        // Let the last keystrokes flush through the relay before teardown.
        sleep(2)
        XCTAssertTrue((editor.value as? String ?? "").contains("hello from iPad sim"))
    }

    func testPhase2SeesRemoteEdit() {
        let app = launch()
        waitConnected(app)

        // Reopen the note created in phase 1 (newest first).
        let row = app.cells.firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.tap()
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))

        // The probe appended " +rust" while the app was closed; catch-up
        // must deliver it into the open editor.
        expectation(
            for: NSPredicate(format: "value CONTAINS ' +rust'"), evaluatedWith: editor)
        waitForExpectations(timeout: 15)

        // And live remote→editor: type once more so phase 3 (probe --expect)
        // can confirm the reverse path stayed up after catch-up.
        editor.tap()
        editor.typeText(" done")
        sleep(2)
    }
}
