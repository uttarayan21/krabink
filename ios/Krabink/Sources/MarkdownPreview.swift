// Read-only rendered view of the note's markdown (the "Preview" toggle):
// headings, lists, quotes, code, tables through `MarkdownRenderer`. No
// ink: page ink is anchored to source lines, which the rendered layout
// does not have. Images render as their alt text.

import SwiftUI
import UIKit

struct MarkdownPreview: UIViewRepresentable {
    let model: NoteModel
    /// Passed in (not read from the store) so a switch re-runs
    /// `updateUIView` and the colours follow.
    let flavor: ThemeFlavor

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.isSelectable = true
        view.font = .systemFont(ofSize: 16)
        view.accessibilityIdentifier = "preview"
        view.textContainerInset = UIEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        view.backgroundColor = .themeSurface
        view.tintColor = .themeAccent
        context.coordinator.render(model.text, flavor: flavor, into: view)
    }

    @MainActor
    final class Coordinator {
        /// What the view currently shows, so an unrelated SwiftUI update
        /// does not re-render the whole note.
        private var shown: (text: String, flavor: ThemeFlavor)?

        func render(_ text: String, flavor: ThemeFlavor, into view: UITextView) {
            if let shown, shown.text == text, shown.flavor == flavor { return }
            shown = (text, flavor)
            var renderer = MarkdownRenderer(image: { _, _ in nil })
            renderer.bodyFont = .systemFont(ofSize: 16)
            renderer.textColor = .themeText
            view.attributedText = renderer.render(text).text
        }
    }
}
