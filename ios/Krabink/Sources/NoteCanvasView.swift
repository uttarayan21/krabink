// The note surface: one text view the keyboard types into and the Pencil
// draws on. Under it, pinned to the same frame, a Metal view draws the
// page ink cleared to the paper colour (opaque: the highlighter multiplies
// against the paper, which a transparent overlay could not do); above it
// the text view is transparent, so the ink shows through under the text.
//
// View stack:
//   NoteCanvasView (paper background; first responder for the tool picker)
//   ├─ metal: MTKView            "noteCanvas", frame = bounds, no touches
//   └─ textView: UITextView      "editor", TextKit 1, clear background
//         gestures: PenGestureRecognizer, UIHoverGestureRecognizer (pencil)
//         UIScribbleInteraction refused, layoutManager.delegate = self
//
// The text view's content coordinates are the page: the ink viewport is
// its content offset at zoom 1, so ink scrolls with the text. Fonts are
// fixed at 16 pt (no Dynamic Type): anchor space depends on it.
//
// CRDT binding, both ways, through one reconciliation (`reconcile`).
// `shadow` is the text the view and the CRDT last agreed on. A local edit
// (`textViewDidChange`) is the splice from `shadow` to the view; a remote
// one (`textChanged` → `model.text`) the splice from `shadow` to the CRDT.
// When both are pending the local splice is transformed past the remote
// one before it goes into the CRDT, then the merged text comes back into
// the view (only the changed range replaced, cursor remapped). Offsets are
// never taken from a view that lags the CRDT, and the view never holds
// text the CRDT lacks past the next keystroke: that was the drift, where
// a remote edit that landed just before a keystroke shifted every later
// local splice and was dropped from the view. After either direction the
// markdown is restyled and the layout manager's completion re-places the
// ink.

import KrabinkCore
import MetalKit
import PencilKit
import SwiftUI
import UIKit

/// How much of the viewport stays free below the last line, so there is
/// paper to draw on past the text.
private let tailFraction: CGFloat = 0.6

@MainActor
final class NoteCanvasView: UIView, UITextViewDelegate, NSLayoutManagerDelegate,
    UIScribbleInteractionDelegate
{
    let metal: MTKView
    let renderer: InkRenderer
    let textView: UITextView
    let pen: PenGestureRecognizer
    let model: NoteModel
    /// Pencil hover (Pencil 2 on M2 iPads, Pencil Pro): the sample under
    /// the tip while it hovers, `nil` when it leaves.
    var onHover: ((RawSample?) -> Void)?
    /// The lines moved (text or width changed); coalesced per run-loop pass.
    var onLayoutChanged: (() -> Void)?
    private let hoverRecognizer = UIHoverGestureRecognizer()
    private var index = ScalarIndex("")
    /// The text the view and the CRDT last agreed on.
    private var shadow = ""
    private var layoutNotifyPending = false
    private var lastTailHeight: CGFloat = -1

    init?(model: NoteModel, policy: InputPolicy) {
        self.model = model
        metal = MTKView(frame: .zero, device: MTLCreateSystemDefaultDevice())
        guard let renderer = InkRenderer(view: metal) else { return nil }
        self.renderer = renderer
        pen = PenGestureRecognizer(policy: policy)
        textView = UITextView(usingTextLayoutManager: false)
        super.init(frame: .zero)

        backgroundColor = .paper

        metal.delegate = renderer
        metal.isPaused = true
        metal.enableSetNeedsDisplay = true
        metal.isUserInteractionEnabled = false
        metal.isOpaque = true
        metal.isAccessibilityElement = true
        metal.accessibilityIdentifier = "noteCanvas"
        addSubview(metal)
        applyBackground()

        textView.backgroundColor = .clear
        textView.textContainerInset = UIEdgeInsets(top: 16, left: 14, bottom: 16, right: 14)
        textView.autocapitalizationType = .none
        textView.autocorrectionType = .no
        textView.smartQuotesType = .no
        textView.smartDashesType = .no
        textView.smartInsertDeleteType = .no
        textView.accessibilityIdentifier = "editor"
        textView.font = MarkdownStyler.bodyFont
        textView.typingAttributes = MarkdownStyler.baseAttributes
        textView.delegate = self
        textView.layoutManager.delegate = self
        textView.layoutManager.allowsNonContiguousLayout = false
        let direct = [NSNumber(value: UITouch.TouchType.direct.rawValue)]
        switch policy {
        case .pencilOnly:
            // The Pencil never scrolls; fingers do.
            textView.panGestureRecognizer.allowedTouchTypes =
                direct + [NSNumber(value: UITouch.TouchType.indirectPointer.rawValue)]
        case .anyInput:
            // One finger inks; two scroll.
            textView.panGestureRecognizer.minimumNumberOfTouches = 2
        }
        textView.addInteraction(UIScribbleInteraction(delegate: self))
        addSubview(textView)
        applyTheme()

        pen.canvasSpace = textView
        textView.addGestureRecognizer(pen)
        hoverRecognizer.addTarget(self, action: #selector(hoverChanged))
        hoverRecognizer.allowedTouchTypes = [NSNumber(value: UITouch.TouchType.pencil.rawValue)]
        textView.addGestureRecognizer(hoverRecognizer)

        setText(model.text)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var canBecomeFirstResponder: Bool { true }

    // MARK: layout

    override func didMoveToWindow() {
        super.didMoveToWindow()
        applyDrawableScale()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        metal.frame = bounds
        textView.frame = bounds
        let tail = (bounds.height * tailFraction).rounded()
        if tail != lastTailHeight {
            lastTailHeight = tail
            var inset = textView.textContainerInset
            inset.bottom = max(16, tail)
            textView.textContainerInset = inset
        }
        applyDrawableScale()
        pushViewport()
        scheduleLayoutNotify()
    }

    /// Render at the screen's native pixel density. An MTKView created
    /// off-window keeps a 1x drawable otherwise, which no amount of MSAA
    /// can hide.
    private func applyDrawableScale() {
        let scale = window?.screen.nativeScale ?? traitCollection.displayScale
        guard scale > 0, metal.contentScaleFactor != scale else { return }
        metal.contentScaleFactor = scale
        renderer.needsDisplay()
    }

    private func pushViewport() {
        renderer.viewport = Viewport(zoom: 1, offset: textView.contentOffset, size: bounds.size)
    }

    /// The layout the ink model resolves anchors against.
    var lineLayout: LineLayout { LineLayout(textView: textView, index: index) }

    private func scheduleLayoutNotify() {
        guard !layoutNotifyPending else { return }
        layoutNotifyPending = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            layoutNotifyPending = false
            onLayoutChanged?()
        }
    }

    nonisolated func layoutManager(
        _ layoutManager: NSLayoutManager, didCompleteLayoutFor textContainer: NSTextContainer?,
        atEnd layoutFinishedFlag: Bool
    ) {
        MainActor.assumeIsolated { scheduleLayoutNotify() }
    }

    // MARK: theme

    /// The flavour the paper was last cleared for.
    private var paperFlavor: ThemeFlavor?

    /// Colours for the current flavour: the paper the ink layer clears to
    /// (and the highlighter blend for it), the text's palette.
    func applyTheme() {
        textView.tintColor = .themeAccent
        textView.keyboardAppearance = ThemeStore.shared.flavor.colorScheme == .dark ? .dark : .light
        let flavor = ThemeStore.shared.flavor
        guard flavor != paperFlavor else { return }
        paperFlavor = flavor
        backgroundColor = .paper
        applyBackground()
        restyle()
    }

    private func applyBackground() {
        metal.clearColor = InkRenderer.clearColor(for: .paper, trait: traitCollection)
        renderer.darkPaper = InkRenderer.isDark(metal.clearColor)
        renderer.needsDisplay()
    }

    override func traitCollectionDidChange(_ previous: UITraitCollection?) {
        super.traitCollectionDidChange(previous)
        if traitCollection.hasDifferentColorAppearance(comparedTo: previous) {
            paperFlavor = nil
            applyTheme()
        }
    }

    // MARK: text

    private func setText(_ text: String) {
        textView.text = text
        shadow = text
        index = ScalarIndex(text)
        restyle()
    }

    /// Restyle the whole text from the core's style runs. Skipped while an
    /// input method composes (marked text): attribute edits would break
    /// the composition.
    private func restyle() {
        guard textView.markedTextRange == nil else { return }
        let text = textView.text ?? ""
        if index.utf16Count != (text as NSString).length { index = ScalarIndex(text) }
        MarkdownStyler.restyle(textView.textStorage, runs: styleRuns(text: text), index: index)
        textView.typingAttributes = MarkdownStyler.baseAttributes
        scheduleLayoutNotify()
    }

    /// The model's text changed under us (a remote edit landed): fold it
    /// in. Cheap when nothing is pending.
    func syncFromModel() {
        guard model.text != shadow || textView.text != shadow else { return }
        reconcile()
    }

    /// Bring the view and the CRDT to the same text. Local changes since
    /// `shadow` go into the CRDT, moved past any remote change that landed
    /// meanwhile; the CRDT's text then comes back into the view.
    private func reconcile() {
        let view = textView.text ?? ""
        guard let crdt = try? model.session.text() else { return }
        if let local = TextSplice.of(shadow, view) {
            var splice = local
            if crdt != shadow, let remote = TextSplice.of(shadow, crdt) {
                splice = local.transformed(past: remote)
            }
            model.localEdit(at: UInt64(splice.at), del: UInt64(splice.del), insert: splice.insert)
        }
        let merged = (try? model.session.text()) ?? crdt
        if merged != view { replaceInView(with: merged) }
        shadow = merged
        model.text = merged
        index = ScalarIndex(merged)
        restyle()
    }

    /// Replace only the range that differs so the local cursor survives.
    private func replaceInView(with target: String) {
        let old = Array((textView.text ?? "").utf16)
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
        let replacement = String(
            utf16CodeUnits: Array(new[prefix..<(new.count - suffix)]),
            count: new.count - suffix - prefix)

        var selection = textView.selectedRange
        textView.textStorage.replaceCharacters(in: range, with: replacement)

        let delta = replacement.utf16.count - range.length
        if selection.location >= range.location + range.length {
            selection.location += delta
        } else if selection.location > range.location {
            selection.location = range.location + replacement.utf16.count
            selection.length = 0
        }
        textView.selectedRange = selection
    }

    // MARK: UITextViewDelegate

    /// Every edit the view made (typing, paste, autocorrect, undo,
    /// dictation) lands here; the splice is taken from the text itself,
    /// so nothing the view does can slip past the CRDT.
    nonisolated func textViewDidChange(_ textView: UITextView) {
        MainActor.assumeIsolated { reconcile() }
    }

    /// The keyboard went away: keep the tool picker by taking the
    /// responder chain back.
    nonisolated func textViewDidEndEditing(_ textView: UITextView) {
        MainActor.assumeIsolated { _ = becomeFirstResponder() }
    }

    nonisolated func scrollViewDidScroll(_ scrollView: UIScrollView) {
        MainActor.assumeIsolated { pushViewport() }
    }

    // MARK: pencil

    /// Scribble would turn pen strokes into text; the pen draws here.
    nonisolated func scribbleInteraction(
        _ interaction: UIScribbleInteraction, shouldBeginAt location: CGPoint
    ) -> Bool {
        false
    }

    @objc private func hoverChanged(_ gesture: UIHoverGestureRecognizer) {
        switch gesture.state {
        case .began, .changed:
            let location = gesture.location(in: textView)
            var roll: CGFloat = 0
            if #available(iOS 17.5, *) { roll = gesture.rollAngle }
            let tilt = Tilt(
                azimuth: Float(gesture.azimuthAngle(in: textView)),
                altitude: Float(gesture.altitudeAngle), roll: Float(roll))
            onHover?(
                RawSample(
                    x: Float(location.x), y: Float(location.y), force: 0.5, tMs: 0, tilt: tilt,
                    estimationId: nil, expectsUpdate: false))
        default:
            onHover?(nil)
        }
    }
}

/// `LineLayoutProvider` over the canvas's current layout; the ink model
/// holds it weakly, the canvas owns it.
@MainActor
final class CanvasLineLayout: LineLayoutProvider {
    private weak var canvas: NoteCanvasView?

    init(canvas: NoteCanvasView) { self.canvas = canvas }

    func line(at point: CGPoint) -> (scalar: Int, origin: CGPoint) {
        canvas?.lineLayout.line(at: point) ?? (0, .zero)
    }

    func origin(forScalar scalar: Int) -> CGPoint {
        canvas?.lineLayout.origin(forScalar: scalar) ?? .zero
    }
}

struct NoteCanvas: UIViewRepresentable {
    let model: NoteModel
    /// Passed in (not read from the store) so a switch re-runs
    /// `updateUIView` and the paper and text colours follow.
    let flavor: ThemeFlavor

    func makeCoordinator() -> Coordinator { Coordinator(model: model.ink) }

    func makeUIView(context: Context) -> UIView {
        guard let canvas = NoteCanvasView(model: model, policy: .launch) else {
            // No Metal device: nothing to draw with. Leave a labelled blank so
            // the note still opens.
            let fallback = UILabel()
            fallback.text = "Metal is unavailable on this device"
            fallback.textAlignment = .center
            fallback.accessibilityIdentifier = "noteCanvas"
            return fallback
        }
        let ink = model.ink
        canvas.pen.onPhase = { phase in
            switch phase {
            case .began(let sample): ink.penBegan(sample)
            case .moved(let coalesced, let predicted):
                ink.penMoved(coalesced: coalesced, predicted: predicted)
            case .estimateUpdated(let samples): ink.penEstimateUpdated(samples)
            case .ended: ink.penEnded(cancelled: false)
            case .cancelled: ink.penEnded(cancelled: true)
            }
        }
        canvas.onHover = { ink.hover($0) }
        canvas.onLayoutChanged = { ink.layoutChanged() }
        ink.hapticView = canvas

        // PencilKit's picker works with any first responder; it hands us
        // the selected tool through the observer. `-tool` pins the tool
        // instead (UI tests).
        if ink.toolOverride == nil {
            let picker = Self.makePicker()
            picker.setVisible(true, forFirstResponder: canvas)
            picker.setVisible(true, forFirstResponder: canvas.textView)
            picker.addObserver(context.coordinator)
            context.coordinator.picker = picker
            canvas.becomeFirstResponder()
            if let selected = context.coordinator.selectedPick { ink.picked = selected }
        }

        let layout = CanvasLineLayout(canvas: canvas)
        context.coordinator.layout = layout
        ink.attach(renderer: canvas.renderer, layout: layout)
        return canvas
    }

    func updateUIView(_ view: UIView, context: Context) {
        guard let canvas = view as? NoteCanvasView else { return }
        canvas.applyTheme()
        canvas.syncFromModel()
    }

    static func dismantleUIView(_ view: UIView, coordinator: Coordinator) {
        coordinator.model.pointerGone()
        coordinator.model.detach()
    }

    /// iOS 18: our own item list — every ink the core honours plus the
    /// vector eraser; watercolour is left out rather than faked. iOS 17:
    /// the stock picker, with watercolour mapped to a lighter marker.
    private static func makePicker() -> PKToolPicker {
        let picker: PKToolPicker
        if #available(iOS 18.0, *) {
            picker = PKToolPicker(toolItems: [
                PKToolPickerInkingItem(type: .pen),
                PKToolPickerInkingItem(type: .pencil),
                PKToolPickerInkingItem(type: .marker),
                PKToolPickerInkingItem(type: .monoline),
                PKToolPickerInkingItem(type: .fountainPen),
                PKToolPickerInkingItem(type: .crayon),
            ] + BrushLibrary.shared.pickerItems() + [
                PKToolPickerEraserItem(type: .vector),
            ])
        } else {
            picker = PKToolPicker()
        }
        picker.stateAutosaveName = "sketch"
        return picker
    }

    @MainActor
    final class Coordinator: NSObject, PKToolPickerObserver {
        let model: PageInkModel
        var picker: PKToolPicker?
        /// Kept alive for the ink model, which only holds it weakly.
        var layout: CanvasLineLayout?

        init(model: PageInkModel) { self.model = model }

        /// The picker's selection as a brush or the eraser; `nil` for an
        /// item the core has no brush for.
        var selectedPick: PickedTool? {
            guard let picker else { return nil }
            if #available(iOS 18.0, *) {
                switch picker.selectedToolItem {
                case let inking as PKToolPickerInkingItem:
                    return .ink(StrokeCodec.selection(inking: inking.inkingTool))
                case let eraser as PKToolPickerEraserItem:
                    return .eraser(eraser.eraserTool)
                case let custom as PKToolPickerCustomItem:
                    return StrokeCodec.selection(item: custom).map(PickedTool.ink)
                default:
                    return nil
                }
            }
            switch picker.selectedTool {
            case let inking as PKInkingTool: return .ink(StrokeCodec.selection(inking: inking))
            case let eraser as PKEraserTool: return .eraser(eraser)
            default: return nil
            }
        }

        nonisolated func toolPickerSelectedToolDidChange(_ toolPicker: PKToolPicker) {
            MainActor.assumeIsolated {
                if let selected = selectedPick { model.picked = selected }
            }
        }

        @available(iOS 18.0, *)
        nonisolated func toolPickerSelectedToolItemDidChange(_ toolPicker: PKToolPicker) {
            MainActor.assumeIsolated {
                if let selected = selectedPick { model.picked = selected }
            }
        }
    }
}
