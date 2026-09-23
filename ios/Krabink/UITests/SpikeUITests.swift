// iM2 spike driver: draws two strokes on the spike canvas and records the
// spike's findings. Only "the canvas accepted two strokes" is asserted —
// the counters themselves are FINDINGS, logged for a human verdict, not
// pass/fail criteria.

import XCTest

final class SpikeUITests: XCTestCase {
    func testPencilKitSpike() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-spike", "1"]
        app.launch()

        let canvas = app.descendants(matching: .any)["spikeCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 10), "spike canvas missing")

        drag(canvas, from: CGVector(dx: 0.2, dy: 0.3), to: CGVector(dx: 0.8, dy: 0.45))
        drag(canvas, from: CGVector(dx: 0.3, dy: 0.6), to: CGVector(dx: 0.7, dy: 0.8))

        let status = app.staticTexts["spikeStatus"]
        let committed = NSPredicate(format: "label CONTAINS 'strokes=2'")
        let result = XCTWaiter().wait(
            for: [expectation(for: committed, evaluatedWith: status)], timeout: 15)

        NSLog("IM2-STATUS %@", status.label)
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.lifetime = .keepAlways
        add(shot)

        XCTAssertEqual(result, .completed, "never reached strokes=2; status: \(status.label)")
    }

    /// Slow press-drag so the synthesized touch stream has many samples.
    private func drag(_ element: XCUIElement, from: CGVector, to: CGVector) {
        let start = element.coordinate(withNormalizedOffset: from)
        let end = element.coordinate(withNormalizedOffset: to)
        start.press(
            forDuration: 0.1, thenDragTo: end,
            withVelocity: .slow, thenHoldForDuration: 0.1)
        usleep(400_000)
    }
}
