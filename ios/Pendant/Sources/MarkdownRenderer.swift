// Markdown → NSAttributedString for the read-only preview.
//
// Parsing is Foundation's cmark-gfm (`AttributedString(markdown:)` with the
// `.full` syntax), which yields flat runs tagged with a block intent
// (`presentationIntent`: paragraph, header, list item, quote, code block,
// table cell…) and inline intents (emphasis, code, strikethrough…). This
// file turns those tags into TextKit styling: heading fonts, bullets and
// numbering with hanging indents, quote indents, monospaced code blocks,
// tab-stop tables and thematic breaks. Images are resolved by the caller,
// which is how the preview swaps `pendant://sketch/<id>` embeds for
// thumbnails.

import UIKit

struct MarkdownRenderer {
    struct Output {
        let text: NSAttributedString
        /// Every image the resolver turned into an attachment, in order.
        let images: [(range: NSRange, url: URL)]
    }

    /// Resolve an image to an attachment; nil renders the alt text as a link.
    var image: (URL, String) -> NSTextAttachment?

    var bodyFont = UIFont.preferredFont(forTextStyle: .body)
    var textColor = UIColor.themeText

    func render(_ source: String) -> Output {
        let options = AttributedString.MarkdownParsingOptions(
            allowsExtendedAttributes: false,
            interpretedSyntax: .full,
            failurePolicy: .returnPartiallyParsedIfPossible)
        guard let parsed = try? AttributedString(markdown: source, options: options) else {
            return Output(text: plain(source), images: [])
        }
        var builder = Builder(renderer: self)
        builder.build(blocks(of: parsed))
        return Output(text: builder.out, images: builder.images)
    }

    private func plain(_ s: String) -> NSAttributedString {
        NSAttributedString(string: s, attributes: [.font: bodyFont, .foregroundColor: textColor])
    }

    // MARK: runs → blocks

    fileprivate struct Span {
        let text: String
        let inline: InlinePresentationIntent
        let link: URL?
        let image: URL?
    }

    fileprivate struct Block {
        let intent: PresentationIntent?
        var spans: [Span]
    }

    /// Group consecutive runs that share a block intent; each paragraph,
    /// heading, list-item paragraph or table cell becomes one block.
    private func blocks(of parsed: AttributedString) -> [Block] {
        var blocks: [Block] = []
        for run in parsed.runs {
            let span = Span(
                text: String(parsed[run.range].characters),
                inline: run.inlinePresentationIntent ?? [],
                link: run.link,
                image: run.imageURL)
            if let last = blocks.indices.last, blocks[last].intent == run.presentationIntent {
                blocks[last].spans.append(span)
            } else {
                blocks.append(Block(intent: run.presentationIntent, spans: [span]))
            }
        }
        return blocks
    }

    // MARK: fonts

    fileprivate func font(size: CGFloat, bold: Bool, italic: Bool, mono: Bool) -> UIFont {
        let base: UIFont = mono
            ? .monospacedSystemFont(ofSize: size * 0.92, weight: bold ? .semibold : .regular)
            : .systemFont(ofSize: size, weight: bold ? .semibold : .regular)
        var traits = UIFontDescriptor.SymbolicTraits()
        if bold { traits.insert(.traitBold) }
        if italic { traits.insert(.traitItalic) }
        guard !traits.isEmpty, let descriptor = base.fontDescriptor.withSymbolicTraits(traits)
        else { return base }
        return UIFont(descriptor: descriptor, size: size)
    }

    fileprivate func headingSize(_ level: Int) -> CGFloat {
        let body = bodyFont.pointSize
        switch level {
        case 1: return body * 1.7
        case 2: return body * 1.45
        case 3: return body * 1.25
        case 4: return body * 1.1
        default: return body
        }
    }
}

// MARK: - block layout

extension MarkdownRenderer {
    /// What a block's intent chain says about where it sits.
    fileprivate struct Context {
        var header: Int?
        var code: Bool = false
        var quoteDepth = 0
        /// Enclosing lists, outermost first: (ordered, item ordinal, item identity).
        var lists: [(ordered: Bool, ordinal: Int, item: Int)] = []
        var rule = false
        var table: (id: Int, columns: [PresentationIntent.TableColumn])?
        var headerRow = false
        var row: Int?
        var column: Int?

        init(_ intent: PresentationIntent?) {
            guard let intent else { return }
            // Components come innermost-first (the leaf paragraph/header/cell
            // first). Normalise to outermost-first so a list item pairs with
            // the list that precedes it.
            var components = intent.components
            if let first = components.first, Self.isLeaf(first.kind) {
                components.reverse()
            }
            var pendingOrdered: Bool?
            for component in components {
                switch component.kind {
                case .header(let level): header = level
                case .codeBlock: code = true
                case .blockQuote: quoteDepth += 1
                case .orderedList: pendingOrdered = true
                case .unorderedList: pendingOrdered = false
                case .listItem(let ordinal):
                    lists.append((pendingOrdered ?? false, ordinal, component.identity))
                    pendingOrdered = nil
                case .thematicBreak: rule = true
                case .table(let columns): table = (component.identity, columns)
                case .tableHeaderRow: headerRow = true
                case .tableRow(let index): row = index
                case .tableCell(let index): column = index
                default: break
                }
            }
        }

        private static func isLeaf(_ kind: PresentationIntent.Kind) -> Bool {
            switch kind {
            case .paragraph, .header, .codeBlock, .thematicBreak, .tableCell: return true
            default: return false
            }
        }
    }

    fileprivate struct Builder {
        let renderer: MarkdownRenderer
        var out = NSMutableAttributedString()
        var images: [(range: NSRange, url: URL)] = []
        /// List items that already carry their bullet/number.
        private var markedItems = Set<Int>()

        private let listUnit: CGFloat = 24
        private let quoteUnit: CGFloat = 18

        init(renderer: MarkdownRenderer) { self.renderer = renderer }

        mutating func build(_ blocks: [Block]) {
            var index = 0
            while index < blocks.count {
                let context = Context(blocks[index].intent)
                if let table = context.table {
                    var cells: [(Context, Block)] = [(context, blocks[index])]
                    index += 1
                    while index < blocks.count {
                        let next = Context(blocks[index].intent)
                        guard next.table?.id == table.id else { break }
                        cells.append((next, blocks[index]))
                        index += 1
                    }
                    appendTable(columns: table.columns, cells: cells, context: context)
                    continue
                }
                appendBlock(blocks[index], context: context)
                index += 1
            }
        }

        // MARK: paragraphs, headings, lists, quotes, code

        private mutating func appendBlock(_ block: Block, context: Context) {
            let start = out.length
            if start > 0 { out.append(newline()) }

            let style = NSMutableParagraphStyle()
            style.paragraphSpacing = renderer.bodyFont.pointSize * 0.5
            let quoteIndent = CGFloat(context.quoteDepth) * quoteUnit
            let depth = CGFloat(context.lists.count)
            var indent = quoteIndent + depth * listUnit
            var prefix: NSAttributedString?

            var block = block
            if let item = context.lists.last, !markedItems.contains(item.item) {
                markedItems.insert(item.item)
                let marker: String
                if let box = takeCheckbox(&block) {
                    marker = box
                } else {
                    marker = item.ordered ? "\(item.ordinal)." : bullet(depth: context.lists.count)
                }
                prefix = NSAttributedString(
                    string: marker + "\t", attributes: baseAttributes(context, bold: false))
                style.firstLineHeadIndent = indent - listUnit
                style.tabStops = [NSTextTab(textAlignment: .left, location: indent)]
                style.defaultTabInterval = listUnit
            } else {
                style.firstLineHeadIndent = indent
            }

            if context.code {
                indent += 8
                style.firstLineHeadIndent += 8
                style.paragraphSpacingBefore = 4
            }
            if let level = context.header {
                style.paragraphSpacingBefore = renderer.headingSize(level) * 0.6
            }
            style.headIndent = indent

            if let prefix { out.append(prefix) }
            if context.rule {
                out.append(rule())
            } else {
                out.append(inline(block, context: context))
            }
            out.addAttribute(
                .paragraphStyle, value: style, range: NSRange(location: start, length: out.length - start))
        }

        private func bullet(depth: Int) -> String {
            switch depth {
            case 1: return "•"
            case 2: return "◦"
            default: return "▪"
            }
        }

        private func rule() -> NSAttributedString {
            NSAttributedString(
                string: String(repeating: "─", count: 40),
                attributes: [
                    .font: renderer.font(size: renderer.bodyFont.pointSize, bold: false, italic: false, mono: true),
                    .foregroundColor: UIColor.themeBorder,
                ])
        }

        private func newline() -> NSAttributedString {
            NSAttributedString(string: "\n", attributes: [.font: renderer.bodyFont])
        }

        private func baseAttributes(_ context: Context, bold: Bool) -> [NSAttributedString.Key: Any] {
            let size = context.header.map(renderer.headingSize) ?? renderer.bodyFont.pointSize
            var attributes: [NSAttributedString.Key: Any] = [
                .font: renderer.font(
                    size: size, bold: bold || context.header != nil, italic: false, mono: context.code),
                .foregroundColor: context.quoteDepth > 0 ? UIColor.themeMuted : renderer.textColor,
            ]
            if context.code {
                attributes[.backgroundColor] = UIColor.themeBg
            }
            return attributes
        }

        /// The block's own text with inline styling applied, images swapped
        /// for attachments and task-list checkboxes turned into symbols.
        private mutating func inline(_ block: Block, context: Context) -> NSAttributedString {
            let result = NSMutableAttributedString()
            let size = context.header.map(renderer.headingSize) ?? renderer.bodyFont.pointSize
            for span in block.spans {
                if let url = span.image {
                    if let attachment = renderer.image(url, span.text) {
                        let piece = NSAttributedString(attachment: attachment)
                        images.append((NSRange(location: out.length + result.length, length: piece.length), url))
                        result.append(piece)
                    } else {
                        var attributes = baseAttributes(context, bold: false)
                        attributes[.link] = url
                        result.append(NSAttributedString(
                            string: span.text.isEmpty ? url.absoluteString : span.text,
                            attributes: attributes))
                    }
                    continue
                }
                var attributes = baseAttributes(context, bold: false)
                let bold = span.inline.contains(.stronglyEmphasized) || context.header != nil
                let italic = span.inline.contains(.emphasized)
                let mono = span.inline.contains(.code) || context.code
                attributes[.font] = renderer.font(size: size, bold: bold, italic: italic, mono: mono)
                if span.inline.contains(.code), !context.code {
                    attributes[.backgroundColor] = UIColor.themeBg
                }
                if span.inline.contains(.strikethrough) {
                    attributes[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
                }
                if let link = span.link {
                    attributes[.link] = link
                }
                var text = span.text
                if span.inline.contains(.lineBreak) {
                    text = "\u{2028}"
                } else if span.inline.contains(.softBreak) {
                    text = " "
                } else if !context.code {
                    text = text.replacingOccurrences(of: "\n", with: " ")
                }
                result.append(NSAttributedString(string: text, attributes: attributes))
            }
            if context.code, result.string.hasSuffix("\n") {
                result.deleteCharacters(in: NSRange(location: result.length - 1, length: 1))
            }
            return result
        }

        /// `[ ]` / `[x]` opening a list item: strip it and return ☐ / ☑ to
        /// stand in for the bullet.
        private func takeCheckbox(_ block: inout Block) -> String? {
            guard let first = block.spans.first, first.image == nil else { return nil }
            let symbol: String
            switch first.text.prefix(3).lowercased() {
            case "[ ]": symbol = "☐"
            case "[x]": symbol = "☑"
            default: return nil
            }
            var rest = first.text.dropFirst(3)
            while rest.first == " " { rest = rest.dropFirst() }
            block.spans[0] = Span(text: String(rest), inline: first.inline, link: first.link, image: first.image)
            return symbol
        }

        // MARK: tables

        /// Tab stops per column, widths from the widest cell (capped), header
        /// row in bold, cells aligned as the column declares.
        private mutating func appendTable(
            columns: [PresentationIntent.TableColumn], cells: [(Context, Block)], context: Context
        ) {
            struct Row {
                var header: Bool
                var cells: [Int: NSAttributedString]
            }
            var rows: [Row] = []
            var rowKeys: [String: Int] = [:]
            for (cellContext, block) in cells {
                let key = cellContext.headerRow ? "header" : "row\(cellContext.row ?? -1)"
                let rowIndex: Int
                if let existing = rowKeys[key] {
                    rowIndex = existing
                } else {
                    rowIndex = rows.count
                    rowKeys[key] = rowIndex
                    rows.append(Row(header: cellContext.headerRow, cells: [:]))
                }
                let text = NSMutableAttributedString(attributedString: inline(block, context: cellContext))
                if cellContext.headerRow {
                    text.enumerateAttribute(.font, in: NSRange(location: 0, length: text.length)) {
                        value, range, _ in
                        let size = (value as? UIFont)?.pointSize ?? renderer.bodyFont.pointSize
                        text.addAttribute(
                            .font, value: renderer.font(size: size, bold: true, italic: false, mono: false),
                            range: range)
                    }
                }
                rows[rowIndex].cells[cellContext.column ?? 0] = text
            }

            let columnCount = max(columns.count, rows.map { ($0.cells.keys.max() ?? -1) + 1 }.max() ?? 0)
            let padding: CGFloat = 16
            let maxWidth: CGFloat = 280
            var widths = [CGFloat](repeating: 0, count: columnCount)
            for row in rows {
                for (column, text) in row.cells where column < columnCount {
                    widths[column] = max(widths[column], min(maxWidth, ceil(text.size().width)))
                }
            }
            widths = widths.map { $0 + padding }

            var tabs: [NSTextTab] = []
            var x: CGFloat = 0
            for column in 0..<columnCount {
                if column > 0 {
                    let alignment = columns.indices.contains(column) ? columns[column].alignment : .left
                    let location: CGFloat
                    switch alignment {
                    case .center: location = x + widths[column] / 2
                    case .right: location = x + widths[column] - padding / 2
                    default: location = x
                    }
                    tabs.append(NSTextTab(textAlignment: alignment.textAlignment, location: location))
                }
                x += widths[column]
            }

            let quoteIndent = CGFloat(context.quoteDepth) * quoteUnit + CGFloat(context.lists.count) * listUnit
            for (index, row) in rows.enumerated() {
                let start = out.length
                if start > 0 { out.append(newline()) }
                for column in 0..<columnCount {
                    if column > 0 { out.append(NSAttributedString(string: "\t", attributes: [.font: renderer.bodyFont])) }
                    if let cell = row.cells[column] { out.append(cell) }
                }
                let style = NSMutableParagraphStyle()
                style.tabStops = tabs
                style.defaultTabInterval = 0
                style.headIndent = quoteIndent
                style.firstLineHeadIndent = quoteIndent
                style.lineBreakMode = .byTruncatingTail
                style.paragraphSpacing = index == rows.count - 1 ? renderer.bodyFont.pointSize * 0.5 : 2
                out.addAttribute(
                    .paragraphStyle, value: style, range: NSRange(location: start, length: out.length - start))
            }
        }
    }
}

extension PresentationIntent.TableColumn.Alignment {
    fileprivate var textAlignment: NSTextAlignment {
        switch self {
        case .center: return .center
        case .right: return .right
        default: return .left
        }
    }
}
