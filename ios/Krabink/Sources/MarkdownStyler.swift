// Styles markdown source in place: the text view shows exactly the CRDT
// text (every marker visible) with headings large, markers dimmed, lists
// indented, code monospaced. The runs come from the core (`styleRuns`,
// unicode-scalar offsets; sorted by start, outer before inner, block
// kinds covering whole lines) and are applied as TextKit attributes over
// UTF-16 ranges through a `ScalarIndex`.
//
// Attribute-only edits do not fire `textViewDidChange` and leave the
// selection alone, so a restyle after every keystroke is invisible to
// the CRDT binding. Whole-text restyle per edit for now (fine to ~50 KB);
// an incremental restyle is a follow-up.
//
// In the reading view an inline sketch embed line (`SketchEmbed` run) is
// laid out but not shown: tiny transparent glyphs on a row as tall as
// the sketch's box, so the box sits in the text flow and the view text
// still equals the CRDT text. The box itself is drawn by
// `InlineBoxOverlay`. In the editor the line is plain link text.

import KrabinkCore
import UIKit

@MainActor
enum MarkdownStyler {
    /// Body font size. Anchor space is defined at this size on every
    /// platform, so no Dynamic Type.
    static let bodySize: CGFloat = 16
    /// Heading sizes by level, in points.
    static let headingSizes: [CGFloat] = [28, 24, 20, 18, 16, 16]
    /// Indent per list level.
    static let listIndent: CGFloat = 20
    /// Hanging indent of a list item's wrapped lines past its marker.
    static let listHang: CGFloat = 18
    static let quoteIndent: CGFloat = 18
    static let codeBlockIndent: CGFloat = 12
    /// Font size of a hidden embed line: small enough never to wrap.
    static let embedFontSize: CGFloat = 8

    static var bodyFont: UIFont { .systemFont(ofSize: bodySize) }

    static var baseParagraph: NSParagraphStyle {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacing = 4
        return style
    }

    /// What plain text looks like; also the text view's typing attributes.
    static var baseAttributes: [NSAttributedString.Key: Any] {
        [
            .font: bodyFont,
            .foregroundColor: UIColor.themeText,
            .paragraphStyle: baseParagraph,
        ]
    }

    /// Reset `storage` to the base look and apply `runs` over it.
    /// `boxHeights` gives each inline sketch's box height (by sketch id;
    /// an embed without one gets the minimum) in the reading view; `nil`
    /// is the editor, where an embed line reads as a link.
    static func restyle(
        _ storage: NSTextStorage, runs: [StyleRun], index: ScalarIndex,
        boxHeights: [String: CGFloat]? = nil
    ) {
        let whole = NSRange(location: 0, length: storage.length)
        storage.beginEditing()
        storage.setAttributes(baseAttributes, range: whole)
        for run in runs {
            let range = utf16Range(run, index: index, length: storage.length)
            guard range.length > 0 else { continue }
            apply(run.kind, to: storage, range: range, boxHeights: boxHeights)
        }
        storage.endEditing()
    }

    /// The inline sketches among `runs`, in text order: sketch id and the
    /// scalar the embed starts at (in the text the runs were made for).
    static func embeds(in runs: [StyleRun]) -> [(sketch: String, scalar: Int)] {
        runs.compactMap { run in
            if case .sketchEmbed(let sketch) = run.kind { return (sketch, Int(run.start)) }
            return nil
        }
    }

    private static func utf16Range(_ run: StyleRun, index: ScalarIndex, length: Int) -> NSRange {
        let start = min(index.utf16(ofScalar: Int(run.start)), length)
        let end = min(index.utf16(ofScalar: Int(run.end)), length)
        return NSRange(location: start, length: max(0, end - start))
    }

    private static func apply(
        _ kind: StyleKind, to storage: NSTextStorage, range: NSRange, boxHeights: [String: CGFloat]?
    ) {
        switch kind {
        case .heading(let level):
            let size = headingSizes[max(0, min(Int(level) - 1, headingSizes.count - 1))]
            setFont(storage, range: range) { font in
                font.withSize(size).adding(.traitBold)
            }
            editParagraphs(storage, range: range) { $0.paragraphSpacingBefore = size * 0.5 }
        case .strong:
            setFont(storage, range: range) { $0.adding(.traitBold) }
        case .emphasis:
            setFont(storage, range: range) { $0.adding(.traitItalic) }
        case .strikethrough:
            storage.addAttribute(.strikethroughStyle, value: NSUnderlineStyle.single.rawValue, range: range)
        case .codeSpan:
            setFont(storage, range: range) { $0.monospaced() }
            storage.addAttribute(.backgroundColor, value: UIColor.themeSurfaceRaised, range: range)
        case .codeBlock:
            let paragraphs = (storage.string as NSString).paragraphRange(for: range)
            setFont(storage, range: paragraphs) { $0.monospaced() }
            storage.addAttribute(.backgroundColor, value: UIColor.themeSurfaceRaised, range: paragraphs)
            editParagraphs(storage, range: range) {
                $0.firstLineHeadIndent = codeBlockIndent
                $0.headIndent = codeBlockIndent
            }
        case .listItem(let depth, _):
            let indent = listIndent * CGFloat(max(1, Int(depth)))
            editParagraphs(storage, range: range) {
                $0.firstLineHeadIndent = indent
                $0.headIndent = indent + listHang
            }
        case .blockQuote:
            setFont(storage, range: range) { $0.adding(.traitItalic) }
            storage.addAttribute(.foregroundColor, value: UIColor.themeMuted, range: range)
            editParagraphs(storage, range: range) {
                $0.firstLineHeadIndent += quoteIndent
                $0.headIndent += quoteIndent
            }
        case .link:
            storage.addAttributes(
                [
                    .foregroundColor: UIColor.themeAccent,
                    .underlineStyle: NSUnderlineStyle.single.rawValue,
                ], range: range)
        case .sketchEmbed(let sketch):
            guard let boxHeights else {
                // Editor: the embed line is source text like any other.
                storage.addAttributes(
                    [
                        .foregroundColor: UIColor.themeAccent,
                        .underlineStyle: NSUnderlineStyle.single.rawValue,
                    ], range: range)
                return
            }
            // Reading view: the row is the sketch's box, the glyphs invisible.
            let height = boxHeights[sketch] ?? InlineGeometry.minHeight
            storage.addAttributes(
                [
                    .font: UIFont.systemFont(ofSize: embedFontSize),
                    .foregroundColor: UIColor.clear,
                ], range: range)
            storage.removeAttribute(.underlineStyle, range: range)
            editParagraphs(storage, range: range) {
                $0.minimumLineHeight = height
                $0.maximumLineHeight = height
                $0.lineBreakMode = .byClipping
            }
        case .marker, .thematicBreak:
            storage.addAttribute(.foregroundColor, value: UIColor.themeMuted, range: range)
            storage.removeAttribute(.underlineStyle, range: range)
        }
    }

    /// Replace every font in `range` with `transform` of it.
    private static func setFont(_ storage: NSTextStorage, range: NSRange, _ transform: (UIFont) -> UIFont) {
        storage.enumerateAttribute(.font, in: range) { value, sub, _ in
            let font = (value as? UIFont) ?? bodyFont
            storage.addAttribute(.font, value: transform(font), range: sub)
        }
    }

    /// Edit the paragraph style of every paragraph touching `range`.
    private static func editParagraphs(
        _ storage: NSTextStorage, range: NSRange, _ edit: (NSMutableParagraphStyle) -> Void
    ) {
        let paragraphs = (storage.string as NSString).paragraphRange(for: range)
        storage.enumerateAttribute(.paragraphStyle, in: paragraphs) { value, sub, _ in
            let style = ((value as? NSParagraphStyle) ?? baseParagraph).mutableCopy() as! NSMutableParagraphStyle
            edit(style)
            storage.addAttribute(.paragraphStyle, value: style, range: sub)
        }
    }
}

extension UIFont {
    /// This font with `trait` added (bold, italic), same size.
    fileprivate func adding(_ trait: UIFontDescriptor.SymbolicTraits) -> UIFont {
        let traits = fontDescriptor.symbolicTraits.union(trait)
        guard let descriptor = fontDescriptor.withSymbolicTraits(traits) else { return self }
        return UIFont(descriptor: descriptor, size: pointSize)
    }

    /// A monospaced font at this size (slightly smaller: mono glyphs read
    /// larger), keeping a bold trait.
    fileprivate func monospaced() -> UIFont {
        let bold = fontDescriptor.symbolicTraits.contains(.traitBold)
        return .monospacedSystemFont(ofSize: pointSize * 0.92, weight: bold ? .semibold : .regular)
    }
}
