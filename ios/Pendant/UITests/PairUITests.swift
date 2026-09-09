// QR pairing e2e on the simulator. The camera can't be scripted, so the
// scan is simulated with the `-pairURI` launch argument, which routes
// through the same adoptPair() path as a scanned pendant:// URL.
//
// Needs a relay running at PENDANT_TEST_SERVER (default ws://127.0.0.1:8722).

import XCTest

final class PairUITests: XCTestCase {
    func testPairURIConnectsAndShowsQR() {
        let env = ProcessInfo.processInfo.environment
        let server = env["PENDANT_TEST_SERVER"] ?? "ws://127.0.0.1:8722/ws"
        let token = env["PENDANT_TEST_TOKEN"] ?? "demo"
        let uri =
            "pendant://pair?server=\(percentEncode(server))&token=\(percentEncode(token))"

        // No -serverURL/-token: connectivity must come from the pair URI.
        let app = XCUIApplication()
        app.launchArguments = ["-pairURI", uri]
        app.launch()

        let state = app.staticTexts["syncState"]
        expectation(
            for: NSPredicate(format: "label == 'connected'"), evaluatedWith: state)
        waitForExpectations(timeout: 15)

        // Paired device can itself share: settings shows the pairing QR and
        // lists this device in the synced registry (registered on connect).
        let settingsButton = app.buttons["settings"]
        XCTAssertTrue(settingsButton.waitForExistence(timeout: 5))
        settingsButton.tap()
        XCTAssertTrue(app.images["pairQR"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["this device"].waitForExistence(timeout: 5))

        // Scanner sheet opens and closes (the simulator has no camera, so
        // this exercises presentation + the fallback view, not scanning).
        // Form rows are lazy: scroll until the join section materializes.
        let scan = app.buttons["scanPairing"]
        for _ in 0..<4 where !scan.exists {
            app.swipeUp()
        }
        XCTAssertTrue(scan.waitForExistence(timeout: 3))
        scan.tap()
        let cancel = app.buttons["scanCancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 5))
        cancel.tap()
        XCTAssertTrue(app.buttons["scanPairing"].waitForExistence(timeout: 5))
    }

    private func percentEncode(_ raw: String) -> String {
        let unreserved = CharacterSet(
            charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")
        return raw.addingPercentEncoding(withAllowedCharacters: unreserved) ?? raw
    }
}
