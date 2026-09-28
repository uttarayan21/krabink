// Store screenshots: seeds a fresh install with placeholder notes and ink,
// then captures the screens the App Store listing shows. Not a check; run
// it on its own on a clean iPad Pro 13" simulator and export the
// attachments from the result bundle:
//
//   xcodebuild test … -only-testing:KrabinkUITests/ScreenshotUITests
//   xcrun xcresulttool export attachments --path <bundle> --output-path <dir>
//
// Landscape, so the sidebar and the note share the screen. The simulator
// runs under `.anyInput`, so one-finger drags ink; each drag is one
// straight stroke, and shapes are drawn as runs of short strokes.

import XCTest

final class ScreenshotUITests: XCTestCase {
    private var app: XCUIApplication!

    override func setUp() {
        continueAfterFailure = true
        XCUIDevice.shared.orientation = .landscapeLeft
    }

    private func launch(theme: String) {
        app = XCUIApplication()
        app.launchArguments = ["-theme", theme]
        app.launch()
    }

    private var editor: XCUIElement {
        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        return editor
    }

    private func shot(_ name: String) {
        sleep(1)
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }

    private func note(_ source: String) {
        app.buttons["newNote"].tap()
        let editor = editor
        editor.tap()
        editor.typeText(source)
    }

    private func open(_ title: String) {
        app.collectionViews.firstMatch.staticTexts[title].firstMatch.tap()
        _ = editor
    }

    // MARK: ink, in the editor's own points

    private func stroke(_ from: CGPoint, _ to: CGPoint) {
        let size = editor.frame.size
        let a = CGVector(dx: from.x / size.width, dy: from.y / size.height)
        let b = CGVector(dx: to.x / size.width, dy: to.y / size.height)
        editor.coordinate(withNormalizedOffset: a)
            .press(
                forDuration: 0.05, thenDragTo: editor.coordinate(withNormalizedOffset: b),
                withVelocity: .slow, thenHoldForDuration: 0.05)
    }

    private func polyline(_ points: [CGPoint]) {
        for (a, b) in zip(points, points.dropFirst()) { stroke(a, b) }
    }

    /// A slightly open, slightly tilted loop, as a hand circles a word.
    private func loop(center c: CGPoint, rx: CGFloat, ry: CGFloat) {
        let steps = 14
        let points = (0...steps).map { i -> CGPoint in
            let t = -0.4 + Double(i) / Double(steps) * 2.1 * .pi
            let wobble = 1 + 0.06 * sin(3 * t)
            return CGPoint(x: c.x + rx * wobble * cos(t), y: c.y + ry * wobble * sin(t) - 4 * cos(t))
        }
        polyline(points)
    }

    private func arrow(_ from: CGPoint, _ to: CGPoint) {
        let mid = CGPoint(x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 - 18)
        polyline([from, mid, to])
        let angle = atan2(to.y - mid.y, to.x - mid.x)
        for side in [-1.0, 1.0] {
            let a = angle + .pi + side * 0.5
            stroke(to, CGPoint(x: to.x + 18 * cos(a), y: to.y + 18 * sin(a)))
        }
    }

    private func underline(_ from: CGPoint, width: CGFloat) {
        polyline([from, CGPoint(x: from.x + width * 0.55, y: from.y + 3), CGPoint(x: from.x + width, y: from.y - 2)])
    }

    private func star(center c: CGPoint, r: CGFloat) {
        let points = (0...5).map { i -> CGPoint in
            let a = -Double.pi / 2 + Double(i) * 4 * .pi / 5
            return CGPoint(x: c.x + r * cos(a), y: c.y + r * sin(a))
        }
        polyline(points)
    }

    // MARK: placeholder library

    private func seed() {
        note("""
            # Groceries

            - [x] oat milk
            - [x] sourdough
            - [ ] lemons
            - [ ] miso paste
            - [ ] basil
            """)
        note("""
            # Reading list

            1. *The Timeless Way of Building*, Christopher Alexander
            2. *A Pattern Language*
            3. *Designing Data-Intensive Applications*
            4. *The Art of Doing Science and Engineering*

            > Each pattern describes a problem which occurs over and over again.
            """)
        note("""
            # Kitchen garden

            Beds along the south fence, paths wide enough for the wheelbarrow.

            """)
        app.buttons["newSketch"].tap()
        editor.typeText("""

            - tomatoes against the fence
            - herbs by the door
            - **mulch** before the first frost
            """)
        note("""
            # Offsite agenda

            ## Day one

            - 09:30 coffee and **roadmap review**
            - 11:00 sync architecture deep dive
            - 14:00 customer interviews: what we heard
            - 16:00 open questions

            ## Day two

            - [ ] pick three bets for Q4
            - [ ] owners and first milestones
            - [ ] dinner at the harbour

            Notes stay on our own devices and sync *peer to peer*: no account, no cloud.
            """)
    }

    /// Latte first: the ink is dark, so the annotated screens use the
    /// light flavour. Positions are page points measured off an earlier
    /// run on the 13" iPad in landscape.
    func testStoreScreenshots() {
        launch(theme: "latte")
        seed()

        // 1. The agenda, annotated on the page in the reading view.
        app.buttons["previewToggle"].tap()
        underline(CGPoint(x: 198, y: 74), width: 104)
        loop(center: CGPoint(x: 58, y: 196), rx: 62, ry: 22)
        star(center: CGPoint(x: 242, y: 246), r: 12)
        underline(CGPoint(x: 305, y: 350), width: 95)
        underline(CGPoint(x: 309, y: 356), width: 88)
        // A brace over day one pointing at "Q4".
        polyline([
            CGPoint(x: 462, y: 48), CGPoint(x: 472, y: 54), CGPoint(x: 472, y: 88),
            CGPoint(x: 484, y: 94), CGPoint(x: 472, y: 100), CGPoint(x: 472, y: 134),
            CGPoint(x: 462, y: 140),
        ])
        arrow(CGPoint(x: 496, y: 94), CGPoint(x: 630, y: 94))
        loop(center: CGPoint(x: 664, y: 94), rx: 14, ry: 16)
        stroke(CGPoint(x: 670, y: 100), CGPoint(x: 684, y: 114))
        polyline([CGPoint(x: 712, y: 78), CGPoint(x: 698, y: 102), CGPoint(x: 722, y: 102)])
        stroke(CGPoint(x: 716, y: 86), CGPoint(x: 716, y: 116))
        shot("01-agenda-reading")

        // 2. Inline sketch in the garden note: fence, three planted beds, sun.
        open("Kitchen garden")
        app.buttons["previewToggle"].tap()
        let status = app.staticTexts["sketchStatus"]
        let boxTop = status.exists
            ? CGFloat(status.label.firstMatch(of: /boxY=(\d+)/).flatMap { Int($0.output.1) } ?? 100)
            : 100
        let x0: CGFloat = 60, y0 = boxTop + 30
        stroke(CGPoint(x: x0, y: y0), CGPoint(x: x0 + 520, y: y0))
        for i in 0..<3 {
            let x = x0 + 20 + CGFloat(i) * 170
            polyline([
                CGPoint(x: x, y: y0 + 25), CGPoint(x: x + 140, y: y0 + 25),
                CGPoint(x: x + 140, y: y0 + 95), CGPoint(x: x, y: y0 + 95), CGPoint(x: x, y: y0 + 25),
            ])
            for j in 0..<3 {
                let c = CGPoint(x: x + 30 + CGFloat(j) * 40, y: y0 + 60)
                polyline([
                    CGPoint(x: c.x - 8, y: c.y - 8), CGPoint(x: c.x, y: c.y + 6),
                    CGPoint(x: c.x + 8, y: c.y - 8),
                ])
            }
        }
        let sun = CGPoint(x: x0 + 620, y: y0 + 40)
        loop(center: sun, rx: 26, ry: 26)
        for k in 0..<8 {
            let a = Double(k) * .pi / 4
            stroke(
                CGPoint(x: sun.x + 36 * cos(a), y: sun.y + 36 * sin(a)),
                CGPoint(x: sun.x + 50 * cos(a), y: sun.y + 50 * sin(a)))
        }
        shot("02-garden-sketch")

        // 3. The reading list, marked up.
        open("Reading list")
        app.buttons["previewToggle"].tap()
        loop(center: CGPoint(x: 356, y: 85), rx: 88, ry: 15)
        underline(CGPoint(x: 48, y: 144), width: 282)
        star(center: CGPoint(x: 352, y: 132), r: 11)
        shot("03-reading-list")
    }

    /// Mocha, the dark default: markdown source of a note without ink
    /// (page ink stays where it was drawn, so it would not line up with
    /// the source), then settings.
    func testStoreScreenshotsThemeDark() {
        launch(theme: "mocha")
        open("Groceries")
        shot("04-groceries-markdown-dark")

        app.buttons["settings"].tap()
        XCTAssertTrue(app.buttons["settingsDone"].waitForExistence(timeout: 5))
        shot("05-settings-dark")
        app.buttons["settingsDone"].tap()
    }
}
