// Where the text's lines are, for ink anchored to them.
//
// The CRDT, anchors and style runs index the text by unicode scalar;
// UIKit by UTF-16 code unit. `ScalarIndex` is the table between the two,
// rebuilt whenever the text changes. `LineLayout` maps a scalar to the top
// of its source line's first line fragment (forward, for placing ink) and
// a point on the page back to the line under it (inverse, for pen-down).
//
// An inline sketch's box is the row of its hidden embed line: `InlineBox`
// is that rect in content space, the sketch's origin inset by the core's
// padding. Pen-down tests boxes before lines.
//
// TextKit 1 on purpose (`UITextView(usingTextLayoutManager: false)`):
// its layout is eager and deterministic, so `lineFragmentRect` is exact.
// TextKit 2 estimates off-screen heights and would make ink jump as the
// estimates settle.

import KrabinkCore
import UIKit

/// Unicode-scalar index ↔ UTF-16 offset for one text.
struct ScalarIndex {
    /// `utf16[i]` is the UTF-16 offset of scalar `i`; one extra entry for
    /// the end of the text.
    private var utf16Offsets: [Int]

    init(_ text: String) {
        var offsets: [Int] = []
        offsets.reserveCapacity(text.unicodeScalars.count + 1)
        var offset = 0
        for scalar in text.unicodeScalars {
            offsets.append(offset)
            offset += scalar.utf16.count
        }
        offsets.append(offset)
        utf16Offsets = offsets
    }

    var scalarCount: Int { utf16Offsets.count - 1 }
    var utf16Count: Int { utf16Offsets[utf16Offsets.count - 1] }

    /// UTF-16 offset of scalar `i` (clamped to the text).
    func utf16(ofScalar i: Int) -> Int {
        utf16Offsets[max(0, min(i, scalarCount))]
    }

    /// The scalar containing UTF-16 offset `u` (a low surrogate maps to
    /// its pair's scalar; clamped to the text).
    func scalar(ofUTF16 u: Int) -> Int {
        let u = max(0, min(u, utf16Count))
        // Last offset ≤ u.
        var low = 0
        var high = scalarCount
        while low < high {
            let mid = (low + high + 1) / 2
            if utf16Offsets[mid] <= u { low = mid } else { high = mid - 1 }
        }
        return low
    }
}

/// One replacement in unicode scalars: `del` scalars at `at` become
/// `insert`. The CRDT's edit unit (`NoteSession.applyTextEdit`).
struct TextSplice: Equatable {
    var at: Int
    var del: Int
    var insert: String
    var insertCount: Int { insert.unicodeScalars.count }

    /// The single splice turning `old` into `new` (common prefix and
    /// suffix trimmed); `nil` when equal.
    static func of(_ old: String, _ new: String) -> TextSplice? {
        guard old != new else { return nil }
        let a = Array(old.unicodeScalars)
        let b = Array(new.unicodeScalars)
        var prefix = 0
        while prefix < a.count && prefix < b.count && a[prefix] == b[prefix] { prefix += 1 }
        var suffix = 0
        while suffix < a.count - prefix && suffix < b.count - prefix
            && a[a.count - 1 - suffix] == b[b.count - 1 - suffix]
        {
            suffix += 1
        }
        var insert = String.UnicodeScalarView()
        insert.append(contentsOf: b[prefix..<(b.count - suffix)])
        return TextSplice(at: prefix, del: a.count - prefix - suffix, insert: String(insert))
    }

    /// This splice, made against the same base text as `remote`, moved to
    /// apply after `remote` did. Disjoint edits shift; when they overlap,
    /// the local insertion is kept and only the part of its deletion the
    /// remote did not already cover is deleted.
    func transformed(past remote: TextSplice) -> TextSplice {
        var out = self
        let remoteEnd = remote.at + remote.del
        let localEnd = at + del
        if remoteEnd <= at {
            out.at += remote.insertCount - remote.del
        } else if remote.at >= localEnd {
            // Remote is after: nothing moves.
        } else if remote.at <= at {
            // Remote starts before or at us: land after its insertion and
            // delete only what remains past its deletion.
            out.at = remote.at + remote.insertCount
            out.del = max(0, localEnd - remoteEnd)
        } else {
            // We start before the remote: delete up to where it begins.
            out.del = remote.at - at
        }
        return out
    }
}

/// The box geometry every platform agrees on (core contract).
enum InlineGeometry {
    /// Inset of a sketch's origin inside its box.
    static let padding: CGFloat = CGFloat(inlinePadding())
    /// Box height of an empty or unknown sketch.
    static let minHeight: CGFloat = CGFloat(inlineMinHeight())
}

/// Where an inline sketch's box is on the page (content space).
struct InlineBox: Equatable {
    let sketch: String
    let rect: CGRect
    /// The sketch's (0, 0): the box corner inset by the padding.
    var origin: CGPoint {
        CGPoint(x: rect.minX + InlineGeometry.padding, y: rect.minY + InlineGeometry.padding)
    }
}

/// What the ink model needs from the editor's layout.
@MainActor
protocol LineLayoutProvider: AnyObject {
    /// The source line under `point` (text view content space): the
    /// scalar index of its first char and its ink origin.
    func line(at point: CGPoint) -> (scalar: Int, origin: CGPoint)
    /// Ink origin of the line containing scalar `scalar` (past the end:
    /// the last line). Content space.
    func origin(forScalar scalar: Int) -> CGPoint
    /// The inline sketch boxes as last laid out, in text order.
    func inlineBoxes() -> [InlineBox]
}

extension LineLayoutProvider {
    /// The box under `point`, if any.
    func inlineBox(at point: CGPoint) -> InlineBox? {
        inlineBoxes().first { $0.rect.contains(point) }
    }
}

/// Line geometry of a TextKit 1 text view. In the reading view the text
/// shown is the source with markers hidden: `sourceOf` maps each display
/// scalar back to its source scalar (`PreviewText.sourceOf`), so the
/// scalars this hands out and takes are always source scalars.
@MainActor
struct LineLayout {
    let textView: UITextView
    /// Over the displayed text.
    let index: ScalarIndex
    /// Display scalar → source scalar; `nil` while editing (identity).
    let sourceOf: [Int]?

    /// Display scalar of the first display char at or after source `scalar`.
    private func display(ofSource scalar: Int) -> Int {
        guard let sourceOf else { return scalar }
        var low = 0
        var high = sourceOf.count
        while low < high {
            let mid = (low + high) / 2
            if sourceOf[mid] < scalar { low = mid + 1 } else { high = mid }
        }
        return low
    }

    private func source(ofDisplay scalar: Int) -> Int {
        guard let sourceOf else { return scalar }
        return sourceOf[max(0, min(scalar, sourceOf.count - 1))]
    }

    private var layoutManager: NSLayoutManager { textView.layoutManager }
    private var container: NSTextContainer { textView.textContainer }
    private var text: NSString { textView.textStorage.string as NSString }

    /// Ink x origin: where a line's glyphs start at zero indent.
    var anchorLeft: CGFloat {
        textView.textContainerInset.left + container.lineFragmentPadding
    }

    /// Start (UTF-16) of the source line containing UTF-16 offset `u`;
    /// `text.length` for the empty line after a trailing newline.
    private func lineStart(utf16 u: Int) -> Int {
        let length = text.length
        let u = max(0, min(u, length))
        return text.lineRange(for: NSRange(location: u, length: 0)).location
    }

    /// Top of the first line fragment of the line starting at `start`,
    /// text container coordinates.
    private func fragmentTop(lineStart start: Int) -> CGFloat {
        let length = text.length
        layoutManager.ensureLayout(for: container)
        if start >= length {
            let extra = layoutManager.extraLineFragmentRect
            if !extra.isEmpty { return extra.minY }
            // No extra fragment: the text does not end in a newline, so
            // the last line is a real fragment (or the text is empty).
            guard length > 0 else { return 0 }
            let glyph = layoutManager.glyphIndexForCharacter(at: length - 1)
            return layoutManager.lineFragmentRect(forGlyphAt: glyph, effectiveRange: nil).minY
        }
        let glyph = layoutManager.glyphIndexForCharacter(at: start)
        return layoutManager.lineFragmentRect(forGlyphAt: glyph, effectiveRange: nil).minY
    }

    private func origin(lineStart start: Int) -> CGPoint {
        CGPoint(x: anchorLeft, y: textView.textContainerInset.top + fragmentTop(lineStart: start))
    }

    func origin(forScalar scalar: Int) -> CGPoint {
        origin(lineStart: lineStart(utf16: index.utf16(ofScalar: display(ofSource: scalar))))
    }

    /// The boxes of `embeds` (sketch id, display scalar of the embed's
    /// first char): each spans the text's width at the top of its line,
    /// `heights[sketch]` tall (the minimum when unknown).
    func inlineBoxes(embeds: [(sketch: String, scalar: Int)], heights: [String: CGFloat]) -> [InlineBox] {
        guard !embeds.isEmpty else { return [] }
        let inset = textView.textContainerInset
        let width = max(0, container.size.width - 2 * container.lineFragmentPadding)
        return embeds.map { embed in
            let start = lineStart(utf16: index.utf16(ofScalar: embed.scalar))
            let top = inset.top + fragmentTop(lineStart: start)
            let height = heights[embed.sketch] ?? InlineGeometry.minHeight
            return InlineBox(
                sketch: embed.sketch,
                rect: CGRect(x: anchorLeft, y: top, width: width, height: height))
        }
    }

    func line(at point: CGPoint) -> (scalar: Int, origin: CGPoint) {
        layoutManager.ensureLayout(for: container)
        let inset = textView.textContainerInset
        let local = CGPoint(x: point.x - inset.left, y: point.y - inset.top)
        let length = text.length
        let used = layoutManager.usedRect(for: container)
        let extra = layoutManager.extraLineFragmentRect
        let start: Int
        if length == 0 || local.y >= used.maxY || (!extra.isEmpty && local.y >= extra.minY) {
            // Below the text, or in the empty last line: the last line.
            start = lineStart(utf16: length)
        } else {
            let glyph = layoutManager.glyphIndex(
                for: CGPoint(x: max(0, local.x), y: max(0, local.y)), in: container,
                fractionOfDistanceThroughGlyph: nil)
            let char = layoutManager.characterIndexForGlyph(at: glyph)
            start = lineStart(utf16: char)
        }
        return (source(ofDisplay: index.scalar(ofUTF16: start)), origin(lineStart: start))
    }
}
