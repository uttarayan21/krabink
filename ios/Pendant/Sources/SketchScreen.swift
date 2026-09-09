// Full-screen sketch canvas. PencilKit is the *input device only*: a
// transparent PKCanvasView whose drawing is kept empty provides pen capture,
// the tool picker and the ruler. Every visible stroke — committed or wet,
// local or remote — is tessellated by the Rust core (`strokeTriangles` /
// `wetTriangles`: the same ribbon geometry the desktop renders) and filled
// into CAShapeLayers on an ink view *under* the canvas. Desktop and iPad
// therefore draw identical ink from identical data.
//
// Local strokes: PencilKit renders the live stroke itself (lowest latency)
// while the active observer streams wet samples onto the ephemeral channel;
// at pen-up canvasViewDrawingDidChange commits the PKStroke to the CRDT
// (reusing the wet id so receivers swap overlay for ink), its ribbon layer
// appears, and the PKDrawing is cleared in the same frame.
// Erasing: the eraser tool's samples hit-test whole strokes in the core.
// Remote strokes: wet batches render as ribbons; strokesChanged diffs the
// CRDT into layers — deferred while a local pen is down, flushed at pen-up.

import PencilKit
import PendantCore
import SwiftUI
import UIKit

/// Wet batches leave the pen at this cadence (plan: 60ms ≈ 4-8 samples).
private let wetFlushInterval: Duration = .milliseconds(60)
/// Backpressure cap: ~2s of 240Hz samples. Overflow drops the oldest —
/// wet ink is lossy-tolerant, the committed stroke is not built from it.
private let wetBufferCap = 480
/// Provisional ink lingers this long after End if the commit never lands
/// (same as the desktop).
private let wetLinger: Duration = .seconds(5)

@Observable @MainActor
final class SketchModel {
    let session: NoteSession
    let sketchId: String
    var strokeCount = 0
    /// Diagnostics surfaced in the status label (log capture on the sim is
    /// unreliable): outbound wet flushes and inbound wet batches this session.
    var wetSent = 0
    var wetRecv = 0

    private weak var canvas: PKCanvasView?
    private weak var ink: InkView?
    /// Committed stroke ids on screen, in CRDT order.
    private var ids: [String] = []
    private var penDown = false
    private var pendingRefresh = false
    private var clearingDrawing = false
    private var erasing = false

    // Outbound wet stream (one local pen at a time).
    private var liveStrokeId: String?
    private var liveSeq: UInt32 = 0
    private var wetBuffer: [WetPoint] = []
    private var flushTask: Task<Void, Never>?
    /// Set at pen-up; `commit` consumes it so the committed stroke keeps the
    /// id receivers already saw on the wet channel.
    private var pendingCommitId: String?

    // Inbound wet overlays, keyed by remote stroke id.
    private var remoteWet: [String: WetLayer] = [:]

    init(session: NoteSession, sketchId: String) {
        self.session = session
        self.sketchId = sketchId
    }

    func attach(_ canvas: PKCanvasView, ink: InkView) {
        self.canvas = canvas
        self.ink = ink
        // The model outlives its canvas (cached per note); a reattach gets a
        // fresh canvas + ink view, so forget what the old one showed.
        ids = []
        remoteWet = [:]
        ink.removeAll()
        refreshFromCrdt()
    }

    func penState(down: Bool) {
        penDown = down
        if !down, pendingRefresh {
            pendingRefresh = false
            refreshFromCrdt()
        }
    }

    // MARK: pen input (active observer)

    /// Pen-down: an inking tool opens the wet stream, the eraser starts
    /// hit-testing. Other tools (lasso, ruler handling) are PencilKit's own.
    func penBegan(at point: CGPoint, force: CGFloat) {
        guard let canvas else { return }
        if canvas.tool is PKEraserTool {
            erasing = true
            erase(at: point)
            return
        }
        guard let tool = canvas.tool as? PKInkingTool else { return }
        liveSeq = 0
        wetBuffer = []
        liveStrokeId = try? session.beginStroke(
            sketch: sketchId,
            tool: StrokeCodec.tool(tool.inkType),
            color: StrokeCodec.pack(tool.color),
            baseWidth: Float(tool.width))
        NSLog("IM4 wet begin id=%@", liveStrokeId ?? "FAILED")
        guard liveStrokeId != nil else { return }
        buffer(point, force: force)
        flushTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: wetFlushInterval)
                self?.flushWet()
            }
        }
    }

    func penMoved(_ samples: [(CGPoint, CGFloat)]) {
        if erasing {
            for (point, _) in samples { erase(at: point) }
            return
        }
        guard liveStrokeId != nil else { return }
        for (point, force) in samples { buffer(point, force: force) }
    }

    /// Pen-up/cancel: flush the tail and hand the id to the upcoming commit.
    /// `finishStroke` (in `commit`) sends the wet End frame. If no stroke
    /// lands (the pen dragged the ruler, not ink), receivers get a Cancel so
    /// the provisional ink does not linger as a phantom stroke.
    func penEnded(cancelled: Bool) {
        if erasing {
            erasing = false
            return
        }
        flushTask?.cancel()
        flushTask = nil
        flushWet()
        guard let id = liveStrokeId else { return }
        liveStrokeId = nil
        if cancelled {
            pendingCommitId = nil
            try? session.cancelStroke(stroke: id)
            return
        }
        pendingCommitId = id
        // PencilKit commits the stroke on the same touch-up; anything still
        // pending shortly after was never a stroke.
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: .milliseconds(150))
            guard let self, self.pendingCommitId == id else { return }
            self.pendingCommitId = nil
            try? self.session.cancelStroke(stroke: id)
        }
    }

    private func buffer(_ point: CGPoint, force: CGFloat) {
        if wetBuffer.count >= wetBufferCap { wetBuffer.removeFirst() }
        wetBuffer.append(
            WetPoint(x: Float(point.x), y: Float(point.y), force: Float(force), width: nil))
    }

    private func flushWet() {
        guard let id = liveStrokeId, !wetBuffer.isEmpty else { return }
        liveSeq += 1
        wetSent += 1
        try? session.appendPoints(stroke: id, seq: liveSeq, points: wetBuffer)
        wetBuffer = []
    }

    // MARK: eraser

    private func erase(at point: CGPoint) {
        guard
            let removed = try? session.eraseAt(
                sketch: sketchId, x: Float(point.x), y: Float(point.y), radius: Float(eraserRadius())),
            !removed.isEmpty
        else { return }
        let gone = Set(removed)
        for id in gone { ink?.remove(id) }
        ids.removeAll { gone.contains($0) }
        strokeCount = ids.count
    }

    private func eraserRadius() -> CGFloat {
        if #available(iOS 16.4, *), let eraser = canvas?.tool as? PKEraserTool {
            return max(4, eraser.width / 2)
        }
        return 12
    }

    // MARK: inbound wet overlay

    func remoteWetBegin(stroke: String, tool: Tool, color: UInt32, baseWidth: Float) {
        guard let ink else { return }
        remoteWet[stroke]?.layer.removeFromSuperlayer()
        let wet = WetLayer(color: StrokeCodec.unpack(color), baseWidth: baseWidth)
        ink.addWet(wet.layer)
        remoteWet[stroke] = wet
    }

    func remoteWetPoints(stroke: String, points: [WetPoint]) {
        wetRecv += 1
        guard let wet = remoteWet[stroke] else { return }
        wet.append(points)
    }

    /// Sender says no stroke is coming: drop the overlay right away.
    func remoteWetCancel(stroke: String) {
        guard let wet = remoteWet.removeValue(forKey: stroke) else { return }
        wet.layer.removeFromSuperlayer()
    }

    func remoteWetEnd(stroke: String) {
        // Keep the overlay until the committed stroke lands (strokesChanged →
        // refresh) so ink never blinks out; drop it only as a fallback when
        // no commit ever arrives.
        guard let wet = remoteWet[stroke] else { return }
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: wetLinger)
            guard let self, self.remoteWet[stroke] === wet else { return }
            self.remoteWet[stroke] = nil
            wet.layer.removeFromSuperlayer()
        }
    }

    // MARK: PencilKit → CRDT → ink layers

    /// PencilKit finished a stroke: commit every stroke it holds, show the
    /// committed ink from the core, and empty the drawing again — the
    /// canvas never keeps ink of its own.
    func drawingChanged() {
        guard !clearingDrawing, let canvas else { return }
        let strokes = canvas.drawing.strokes
        guard !strokes.isEmpty else { return }
        for stroke in strokes { commit(stroke) }
        clearingDrawing = true
        canvas.drawing = PKDrawing()
        clearingDrawing = false
        strokeCount = ids.count
        grow()
    }

    func remoteChanged() {
        if penDown {
            pendingRefresh = true
        } else {
            refreshFromCrdt()
        }
    }

    /// Erase helper for the toolbar (and UI tests): drops the newest stroke
    /// through the same CRDT path the eraser uses.
    func eraseLast() {
        guard let last = ids.last else { return }
        try? session.removeStroke(sketch: sketchId, stroke: last)
        ink?.remove(last)
        ids.removeLast()
        strokeCount = ids.count
    }

    private func commit(_ stroke: PKStroke) {
        let id: String
        if let pending = pendingCommitId {
            // Streamed stroke: reuse the id the wet channel announced so
            // receivers swap their overlay for this committed stroke.
            pendingCommitId = nil
            id = pending
        } else {
            let preview = StrokeCodec.encode(stroke, id: "")
            guard
                let fresh = try? session.beginStroke(
                    sketch: sketchId,
                    tool: preview.tool,
                    color: preview.color,
                    baseWidth: preview.baseWidth)
            else { return }
            id = fresh
        }
        let encoded = StrokeCodec.encode(stroke, id: id)
        try? session.finishStroke(sketch: sketchId, stroke: encoded)
        ink?.show(encoded, z: ids.count)
        ids.append(id)
    }

    private func refreshFromCrdt() {
        guard let ink, let crdt = try? session.strokes(sketch: sketchId) else { return }
        let newIds = crdt.map(\.id)
        if newIds != ids {
            let keep = Set(newIds)
            for id in ids where !keep.contains(id) { ink.remove(id) }
            for (z, stroke) in crdt.enumerated() { ink.show(stroke, z: z) }
            ids = newIds
        }
        // A committed stroke replaces its wet overlay.
        for id in newIds {
            if let wet = remoteWet.removeValue(forKey: id) { wet.layer.removeFromSuperlayer() }
        }
        strokeCount = ids.count
        grow()
    }

    /// Infinite canvas v1: grow content down/right only.
    private func grow() {
        guard let canvas, let ink else { return }
        let drawn = ink.inkBounds
        let margin: CGFloat = 400
        let needed = CGSize(
            width: max(canvas.bounds.width, drawn.maxX + margin),
            height: max(canvas.bounds.height, drawn.maxY + margin))
        if canvas.contentSize.width < needed.width || canvas.contentSize.height < needed.height {
            canvas.contentSize = needed
        }
        ink.frame = CGRect(origin: .zero, size: canvas.contentSize)
    }
}

/// All ink on screen: one CAShapeLayer per committed stroke (z = CRDT
/// order) plus wet overlays on top, each filled with the core's ribbon
/// triangles. The triangles are wound consistently, so the non-zero fill
/// rule reproduces the desktop mesh's coverage exactly.
@MainActor
final class InkView: UIView {
    private var strokes: [String: CAShapeLayer] = [:]
    /// Union of committed ink bounds; drives the infinite-canvas growth.
    private(set) var inkBounds = CGRect.zero

    override init(frame: CGRect) {
        super.init(frame: frame)
        isUserInteractionEnabled = false
        backgroundColor = .clear
        isOpaque = false
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Show a committed stroke (idempotent; re-show only updates z).
    func show(_ stroke: Stroke, z: Int) {
        if let existing = strokes[stroke.id] {
            existing.zPosition = CGFloat(z)
            return
        }
        let shape = CAShapeLayer()
        shape.fillColor = StrokeCodec.unpack(stroke.color).cgColor
        shape.strokeColor = nil
        shape.fillRule = .nonZero
        shape.path = Self.path(strokeTriangles(stroke: stroke))
        shape.zPosition = CGFloat(z)
        layer.addSublayer(shape)
        strokes[stroke.id] = shape
        if let box = shape.path?.boundingBox {
            inkBounds = inkBounds.isEmpty ? box : inkBounds.union(box)
        }
    }

    func remove(_ id: String) {
        strokes.removeValue(forKey: id)?.removeFromSuperlayer()
    }

    func removeAll() {
        for shape in strokes.values { shape.removeFromSuperlayer() }
        strokes = [:]
        layer.sublayers?.forEach { $0.removeFromSuperlayer() }
        inkBounds = .zero
    }

    /// Wet overlays sit above every committed stroke.
    func addWet(_ wet: CALayer) {
        wet.zPosition = 1_000_000
        layer.addSublayer(wet)
    }

    /// Flat `[x0, y0, x1, y1, x2, y2, …]` triangles → one closed subpath each.
    static func path(_ xy: [Float]) -> CGPath {
        let path = CGMutablePath()
        var i = 0
        while i + 5 < xy.count {
            path.move(to: CGPoint(x: CGFloat(xy[i]), y: CGFloat(xy[i + 1])))
            path.addLine(to: CGPoint(x: CGFloat(xy[i + 2]), y: CGFloat(xy[i + 3])))
            path.addLine(to: CGPoint(x: CGFloat(xy[i + 4]), y: CGFloat(xy[i + 5])))
            path.closeSubpath()
            i += 6
        }
        return path
    }
}

/// One in-flight remote stroke, re-tessellated from the core on every
/// batch so it looks exactly like the ink it will become.
@MainActor
final class WetLayer {
    let layer = CAShapeLayer()
    private let baseWidth: Float
    private var points: [WetPoint] = []

    init(color: UIColor, baseWidth: Float) {
        self.baseWidth = baseWidth
        layer.fillColor = color.cgColor
        layer.strokeColor = nil
        layer.fillRule = .nonZero
    }

    func append(_ batch: [WetPoint]) {
        points.append(contentsOf: batch)
        layer.path = InkView.path(wetTriangles(points: points, baseWidth: baseWidth))
    }
}

struct SketchScreen: View {
    let model: SketchModel
    let done: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("strokes=\(model.strokeCount) wetSent=\(model.wetSent) wetRecv=\(model.wetRecv)")
                    .font(.system(size: 13, design: .monospaced))
                    .accessibilityIdentifier("sketchStatus")
                Spacer()
                Button("erase last") { model.eraseLast() }
                    .accessibilityIdentifier("eraseLast")
                Button("done") { done() }
                    .accessibilityIdentifier("sketchDone")
            }
            .padding(8)
            SketchCanvas(model: model)
        }
    }
}

struct SketchCanvas: UIViewRepresentable {
    let model: SketchModel

    func makeCoordinator() -> Coordinator { Coordinator(model: model) }

    func makeUIView(context: Context) -> PKCanvasView {
        let canvas = PKCanvasView()
        // .anyInput so simulator fingers draw; device pass will gate on
        // .pencilOnly so fingers scroll.
        canvas.drawingPolicy = .anyInput
        canvas.tool = PKInkingTool(.pen, color: .black, width: 10)
        canvas.delegate = context.coordinator
        canvas.isAccessibilityElement = true
        canvas.accessibilityIdentifier = "sketchCanvas"
        // PencilKit only shows the stroke being drawn; everything else is
        // the ink view underneath, so the canvas must not paint over it.
        canvas.isOpaque = false
        canvas.backgroundColor = .white

        // All ink (committed + remote wet) lives in content coordinates
        // under PencilKit's transparent drawing view.
        let ink = InkView(frame: CGRect(origin: .zero, size: canvas.contentSize))
        canvas.insertSubview(ink, at: 0)

        // Pen tracking + outbound wet stream ride the active observer
        // variant the iM2 spike validated (passive observers never see
        // pen-up). Touch locations in the scroll view are content coords.
        let observer = ActiveObserverGestureRecognizer(target: nil, action: nil)
        let model = model
        observer.onTouches = { [weak canvas] phase, touches, event in
            guard let canvas, let touch = touches.first else { return }
            switch phase {
            case .began:
                model.penState(down: true)
                model.penBegan(at: touch.location(in: canvas), force: max(touch.force, 0.3))
            case .moved:
                let coalesced = event.coalescedTouches(for: touch) ?? [touch]
                model.penMoved(
                    coalesced.map { ($0.location(in: canvas), max($0.force, 0.3)) })
            case .ended:
                model.penState(down: false)
                model.penEnded(cancelled: false)
            case .cancelled:
                model.penState(down: false)
                model.penEnded(cancelled: true)
            default: break
            }
        }
        canvas.addGestureRecognizer(observer)

        let picker = PKToolPicker()
        picker.setVisible(true, forFirstResponder: canvas)
        picker.addObserver(canvas)
        context.coordinator.picker = picker
        canvas.becomeFirstResponder()

        model.attach(canvas, ink: ink)
        return canvas
    }

    func updateUIView(_ canvas: PKCanvasView, context: Context) {}

    @MainActor
    final class Coordinator: NSObject, PKCanvasViewDelegate {
        let model: SketchModel
        var picker: PKToolPicker?

        init(model: SketchModel) { self.model = model }

        nonisolated func canvasViewDrawingDidChange(_ canvasView: PKCanvasView) {
            MainActor.assumeIsolated { model.drawingChanged() }
        }
    }
}
