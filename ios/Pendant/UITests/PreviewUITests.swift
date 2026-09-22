// Preview mode renders markdown structure (not the raw source): headings
// lose their `#`, list items get bullets/numbers, emphasis markers vanish.
// Runs offline — notes are local until a relay connects.

import XCTest

final class PreviewUITests: XCTestCase {
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

    func testPreviewRendersMarkdown() {
        let app = launch()
        app.buttons["newNote"].tap()

        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        editor.typeText(
            """
            # Title

            Some *emphasis*, **strong** and `code` text with a [link](https://example.com).

            - first
            - second
              - nested

            1. one
            2. two

            - [ ] todo
            - [x] done

            > quoted words

            ```
            let x = 1
            ```

            | name | count |
            | --- | ---: |
            | apples | 3 |

            ---

            ~~struck~~ end.

            """)

        app.buttons["previewToggle"].tap()
        let preview = app.textViews["preview"]
        XCTAssertTrue(preview.waitForExistence(timeout: 5))
        let text = preview.value as? String ?? ""

        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "preview"
        shot.lifetime = .keepAlways
        add(shot)

        XCTAssertTrue(text.contains("Title"), text)
        XCTAssertFalse(text.contains("# Title"), "heading marker leaked: \(text)")
        XCTAssertTrue(text.contains("•\tfirst"), "no bullet: \(text)")
        XCTAssertTrue(text.contains("◦\tnested"), "no nested bullet: \(text)")
        XCTAssertTrue(text.contains("1.\tone"), "no number: \(text)")
        XCTAssertTrue(text.contains("☐\ttodo") || text.contains("☐ todo"), "no checkbox: \(text)")
        XCTAssertTrue(text.contains("☑"), "no checked box: \(text)")
        XCTAssertFalse(text.contains("**"), "strong marker leaked: \(text)")
        XCTAssertFalse(text.contains("```"), "fence leaked: \(text)")
        XCTAssertTrue(text.contains("let x = 1"), text)
        XCTAssertTrue(text.contains("apples\t3"), "table not tabbed: \(text)")
        XCTAssertFalse(text.contains("| ---"), "table rule leaked: \(text)")
        XCTAssertFalse(text.contains("~~"), "strikethrough marker leaked: \(text)")
        XCTAssertFalse(text.contains("](https"), "link syntax leaked: \(text)")
    }
}
