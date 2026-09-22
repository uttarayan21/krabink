// iM5: read-only markdown preview that renders inline sketch embeds as
// tappable thumbnails. The editable source view (MarkdownTextView) keeps a
// 1:1 view↔CRDT text mapping, so attachments live only here in preview.
//
// Each `![alt](pendant://sketch/<id>)` token becomes an NSTextAttachment
// rendered from the sketch's committed elements; tapping it opens the canvas.

import MetalKit
import PendantCore
import SwiftUI
import UIKit

private let sketchURIPrefix = "pendant://sketch/"
/// Matches a markdown image whose URI is a sketch embed; group 1 = sketch id.
private let embedRegex = try! NSRegularExpression(
    pattern: #"!\[[^\]]*\]\(pendant://sketch/([0-9A-Za-z]+)\)"#)

struct SketchPreview: UIViewRepresentable {
    let model: NoteModel
    let openSketch: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(open: openSketch) }

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.isSelectable = true
        view.font = .systemFont(ofSize: 16)
        view.backgroundColor = .themeSurface
        view.textColor = .themeText
        view.tintColor = .themeAccent
        view.accessibilityIdentifier = "preview"
        view.textContainerInset = UIEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        let tap = UITapGestureRecognizer(
            target: context.coordinator, action: #selector(Coordinator.handleTap(_:)))
        view.addGestureRecognizer(tap)
        context.coordinator.view = view
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.open = openSketch
        context.coordinator.render(model)
    }

    @MainActor
    final class Coordinator: NSObject {
        var open: (String) -> Void
        weak var view: UITextView?
        /// Attachment ranges → sketch id, for tap hit-testing.
        private var links: [(range: NSRange, id: String)] = []
        /// Cache thumbnails by id+stroke count so re-render is cheap.
        private var thumbCache: [String: (count: Int, image: UIImage)] = [:]
        private var lastRendered = ""
        /// Offscreen ink renderer: thumbnails are the canvas's own pipeline.
        private lazy var renderer = InkRenderer(
            view: MTKView(frame: .zero, device: MTLCreateSystemDefaultDevice()))

        init(open: @escaping (String) -> Void) { self.open = open }

        func render(_ model: NoteModel) {
            guard let view else { return }
            let source = model.text
            // Re-render only when text changed (thumbnails refresh via the
            // stroke-count key inside buildAttributed).
            if source == lastRendered, !thumbnailsStale(model) { return }
            lastRendered = source
            let (attributed, links) = buildAttributed(source, model: model)
            self.links = links
            view.attributedText = attributed
        }

        private func thumbnailsStale(_ model: NoteModel) -> Bool {
            links.contains { link in
                let count = (try? model.session.elements(sketch: link.id))?.count ?? 0
                return thumbCache[link.id]?.count != count
            }
        }

        private func buildAttributed(
            _ source: String, model: NoteModel
        ) -> (NSAttributedString, [(NSRange, String)]) {
            let out = NSMutableAttributedString()
            let ns = source as NSString
            var links: [(NSRange, String)] = []
            var cursor = 0
            let matches = embedRegex.matches(
                in: source, range: NSRange(location: 0, length: ns.length))
            for match in matches {
                if match.range.location > cursor {
                    out.append(plainText(ns.substring(
                        with: NSRange(location: cursor, length: match.range.location - cursor))))
                }
                let id = ns.substring(with: match.range(at: 1))
                let attachment = NSTextAttachment()
                attachment.image = thumbnail(id: id, model: model)
                let attachStr = NSMutableAttributedString(attachment: attachment)
                let attachRange = NSRange(location: out.length, length: attachStr.length)
                out.append(attachStr)
                links.append((attachRange, id))
                cursor = match.range.location + match.range.length
            }
            if cursor < ns.length {
                out.append(plainText(ns.substring(from: cursor)))
            }
            return (out, links)
        }

        private func plainText(_ s: String) -> NSAttributedString {
            NSAttributedString(
                string: s,
                attributes: [
                    .font: UIFont.systemFont(ofSize: 16),
                    .foregroundColor: UIColor.themeText,
                ])
        }

        /// Render committed elements to a bounded thumbnail through the
        /// canvas's Metal pipeline — the same ink, masks, multiply and
        /// write-once overlap the canvas draws. Empty sketch (or no Metal)
        /// → a placeholder box.
        private func thumbnail(id: String, model: NoteModel) -> UIImage {
            let elements = (try? model.session.elements(sketch: id)) ?? []
            if let cached = thumbCache[id], cached.count == elements.count {
                return cached.image
            }
            let maxSide: CGFloat = 240
            let trait = view?.traitCollection ?? UITraitCollection.current
            let rendered = elements.isEmpty
                ? nil
                : renderer?.renderThumbnail(
                    elements: elements, maxSide: maxSide, background: .paper, trait: trait)
            let image = rendered.map(framed) ?? placeholder(size: CGSize(width: maxSide, height: 120))
            thumbCache[id] = (elements.count, image)
            return image
        }

        private func placeholder(size: CGSize) -> UIImage {
            UIGraphicsImageRenderer(size: size).image { _ in
                let bg = UIBezierPath(
                    roundedRect: CGRect(origin: .zero, size: size).insetBy(dx: 1, dy: 1),
                    cornerRadius: Theme.radius)
                UIColor.themeBg.setFill()
                bg.fill()
                UIColor.themeBorder.setStroke()
                bg.lineWidth = 1
                bg.stroke()
                let label = "tap to sketch" as NSString
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: UIFont.systemFont(ofSize: 14),
                    .foregroundColor: UIColor.themeMuted,
                ]
                let ts = label.size(withAttributes: attrs)
                label.draw(
                    at: CGPoint(x: (size.width - ts.width) / 2, y: (size.height - ts.height) / 2),
                    withAttributes: attrs)
            }
        }

        private func framed(_ image: UIImage) -> UIImage {
            let inset: CGFloat = 6
            let size = CGSize(
                width: image.size.width + inset * 2, height: image.size.height + inset * 2)
            // Paper-coloured tile with the card hairline, so the sketch
            // sits inline on the preview like on the desktop.
            return UIGraphicsImageRenderer(size: size).image { _ in
                let tile = UIBezierPath(
                    roundedRect: CGRect(origin: .zero, size: size).insetBy(dx: 1, dy: 1),
                    cornerRadius: Theme.radius)
                UIColor.paper.setFill()
                tile.fill()
                UIColor.themeBorder.setStroke()
                tile.lineWidth = 1
                tile.stroke()
                image.draw(at: CGPoint(x: inset, y: inset))
            }
        }

        @objc func handleTap(_ gesture: UITapGestureRecognizer) {
            guard let view else { return }
            let point = gesture.location(in: view)
            let layout = view.layoutManager
            let container = view.textContainer
            let inset = view.textContainerInset
            let glyphPoint = CGPoint(x: point.x - inset.left, y: point.y - inset.top)
            // Hit-test against each attachment's actual glyph rect — more
            // reliable than characterIndex over an image glyph.
            for link in links {
                let glyphRange = layout.glyphRange(
                    forCharacterRange: link.range, actualCharacterRange: nil)
                let rect = layout.boundingRect(forGlyphRange: glyphRange, in: container)
                if rect.contains(glyphPoint) {
                    open(link.id)
                    return
                }
            }
        }
    }
}
