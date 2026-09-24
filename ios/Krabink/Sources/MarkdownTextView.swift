// UITextView-backed markdown editor with a two-way CRDT binding.
//
// Local direction: `shouldChangeTextIn` converts the UTF-16 NSRange to
// unicode-scalar offsets (what the CRDT indexes by) and forwards the splice;
// UITextView then applies the edit itself. Remote direction: `updateUIView`
// diffs the view against the model (common prefix/suffix), replaces only the
// changed range and remaps the cursor, so typing survives remote edits.

import KrabinkCore
import SwiftUI
import UIKit

struct MarkdownTextView: UIViewRepresentable {
    let model: NoteModel
    /// Passed in (not read from the store) so a switch re-runs
    /// `updateUIView` and the colours follow.
    let flavor: ThemeFlavor

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.font = .monospacedSystemFont(ofSize: 16, weight: .regular)
        applyTheme(to: view)
        view.textContainerInset = UIEdgeInsets(top: 16, left: 14, bottom: 16, right: 14)
        view.autocapitalizationType = .none
        view.autocorrectionType = .no
        view.smartQuotesType = .no
        view.smartDashesType = .no
        view.accessibilityIdentifier = "editor"
        view.delegate = context.coordinator
        view.text = model.text
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.model = model
        applyTheme(to: view)
        let target = model.text
        guard view.text != target else { return }
        applyRemote(target, to: view)
    }

    private func applyTheme(to view: UITextView) {
        view.backgroundColor = .themeSurface
        view.textColor = .themeText
        view.tintColor = .themeAccent
        view.keyboardAppearance = flavor.colorScheme == .dark ? .dark : .light
    }

    /// Replace only the changed range so the local cursor survives.
    private func applyRemote(_ target: String, to view: UITextView) {
        let old = Array((view.text ?? "").utf16)
        let new = Array(target.utf16)

        var prefix = 0
        while prefix < old.count && prefix < new.count && old[prefix] == new[prefix] {
            prefix += 1
        }
        var suffix = 0
        while suffix < old.count - prefix && suffix < new.count - prefix
            && old[old.count - 1 - suffix] == new[new.count - 1 - suffix]
        {
            suffix += 1
        }

        let range = NSRange(location: prefix, length: old.count - prefix - suffix)
        let replacement = String(utf16CodeUnits: Array(new[prefix..<(new.count - suffix)]),
                                 count: new.count - suffix - prefix)

        var selection = view.selectedRange
        view.textStorage.replaceCharacters(in: range, with: replacement)

        let delta = replacement.utf16.count - range.length
        if selection.location >= range.location + range.length {
            selection.location += delta
        } else if selection.location > range.location {
            selection.location = range.location + replacement.utf16.count
            selection.length = 0
        }
        view.selectedRange = selection
    }

    final class Coordinator: NSObject, UITextViewDelegate {
        var model: NoteModel

        init(model: NoteModel) {
            self.model = model
        }

        func textView(
            _ textView: UITextView,
            shouldChangeTextIn range: NSRange,
            replacementText text: String
        ) -> Bool {
            let ns = (textView.text ?? "") as NSString
            let at = scalarCount(ns.substring(to: range.location))
            let del = scalarCount(ns.substring(with: range))
            model.localEdit(at: UInt64(at), del: UInt64(del), insert: text)
            return true
        }

        func textViewDidChange(_ textView: UITextView) {
            // Keep the model in step so updateUIView sees no phantom diff.
            model.text = textView.text ?? ""
        }

        private func scalarCount(_ s: String) -> Int {
            s.unicodeScalars.count
        }
    }
}
