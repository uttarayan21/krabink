// Preview mode is the reading view: the same editor with the markdown
// syntax hidden (headings lose their `#`, list items get bullets, emphasis
// markers vanish, code fences go) and editing off. Runs offline — notes
// are local until a relay connects.

import XCTest

final class PreviewUITests: XCTestCase {
    private func launch() -> XCUIApplication {
        let app = XCUIApplication()
        let env = ProcessInfo.processInfo.environment
        app.launchArguments = [
            "-pairURI", env["KRABINK_TEST_PAIR"] ?? "",
        ]
        app.launch()
        return app
    }

    func testPreviewHidesMarkdownSyntax() {
        let app = launch()
        app.buttons["newNote"].tap()

        let editor = app.textViews["editor"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        let source = """
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

            ---

            ~~struck~~ end.

            """
        editor.typeText(source)

        app.buttons["previewToggle"].tap()
        // Same text view, now read-only, showing the reading view.
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "preview"
        shot.lifetime = .keepAlways
        add(shot)

        let text = editor.value as? String ?? ""
        XCTAssertTrue(text.contains("Title"), text)
        XCTAssertFalse(text.contains("# Title"), "heading marker leaked: \(text)")
        XCTAssertTrue(text.contains("• first"), "no bullet: \(text)")
        XCTAssertTrue(text.contains("◦ nested"), "no nested bullet: \(text)")
        XCTAssertTrue(text.contains("1. one"), "number lost: \(text)")
        XCTAssertTrue(text.contains("☐ todo"), "no checkbox: \(text)")
        XCTAssertTrue(text.contains("☑ done"), "no checked box: \(text)")
        XCTAssertFalse(text.contains("**"), "strong marker leaked: \(text)")
        XCTAssertFalse(text.contains("```"), "fence leaked: \(text)")
        XCTAssertTrue(text.contains("let x = 1"), text)
        XCTAssertFalse(text.contains("> quoted"), "quote marker leaked: \(text)")
        XCTAssertTrue(text.contains("quoted words"), text)
        XCTAssertFalse(text.contains("~~"), "strikethrough marker leaked: \(text)")
        XCTAssertFalse(text.contains("](https"), "link syntax leaked: \(text)")
        XCTAssertTrue(text.contains("link."), "link text lost: \(text)")

        // Back to the editor: the source is intact.
        app.buttons["previewToggle"].tap()
        let back = editor.value as? String ?? ""
        XCTAssertTrue(back.contains("# Title"), "source lost after preview: \(back)")
        XCTAssertTrue(back.contains("- [ ] todo"), "source lost after preview: \(back)")
    }
}
