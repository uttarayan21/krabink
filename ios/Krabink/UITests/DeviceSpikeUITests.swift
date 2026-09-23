// iM2 hardware half: runs ON A PHYSICAL IPAD and waits for a human to
// draw with the Apple Pencil. Polls the spike status label until at
// least 3 strokes are committed (or times out), then logs the findings.
// Not part of the simulator harness — invoke explicitly with
// -only-testing:KrabinkUITests/DeviceSpikeUITests on a device destination.

import XCTest

final class DeviceSpikeUITests: XCTestCase {
    func testHardwarePencilSpike() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-spike", "1", "-pencilOnly", "1"]
        app.launch()

        let canvas = app.descendants(matching: .any)["spikeCanvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 15), "spike canvas missing")
        NSLog("IM2-HW waiting for a human to draw >=3 pencil strokes")

        let status = app.staticTexts["spikeStatus"]
        let deadline = Date().addingTimeInterval(180)
        var strokes = 0
        while Date() < deadline {
            let label = status.label
            if let range = label.range(of: #"strokes=(\d+)"#, options: .regularExpression),
                let n = Int(label[range].dropFirst("strokes=".count))
            {
                strokes = n
                if n >= 3 { break }
            }
            sleep(3)
        }

        NSLog("IM2-HW-STATUS %@", status.label)
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.lifetime = .keepAlways
        add(shot)
        XCTAssertGreaterThanOrEqual(strokes, 3, "not enough strokes drawn; status: \(status.label)")
    }
}
