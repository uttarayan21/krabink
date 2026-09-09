// Full-screen sketch canvas: whole-stroke CRDT sync (iM3) + live wet-ink
// streaming both ways (iM4).
//
// Local strokes: PencilKit draws; the active observer streams samples onto
// the ephemeral channel in 60ms batches while the pen moves, and
// canvasViewDrawingDidChange commits the authoritative stroke at pen-up
// (reusing the wet stroke id so receivers swap overlay for committed ink).
// Remote strokes: wet batches render as CAShapeLayer polylines on an
// overlay above the canvas; strokesChanged rebuilds the drawing from the
// CRDT — deferred while a local pen is down, flushed at pen-up.

import PencilKit
import PendantCore
import SwiftUI
import UIKit

/// Wet batches leave the pen at this cadence (plan: 60ms ≈ 4-8 samples).
private let wetFlushInterval: Duration = .milliseconds(60)
/// Backpressure cap: ~2s of 240Hz samples. Overflow drops the oldest —
/// wet ink is lossy-tolerant, the committed stroke is not built from it.
private let wetBufferCap = 480

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
    private weak var overlay: UIView?
    /// CRDT stroke ids aligned with `canvas.drawing.strokes` order.
    private var ids: [String] = []
    /// PKStroke has no stable public identity; key strokes by their path
    /// creationDate (sub-ms for local strokes, whole-ms for decoded remote
    /// ones — collisions are theoretically possible, tolerated for now).
    private var keyToId: [Date: String] = [:]
    private var penDown = false
    private var pendingRefresh = false
    private var applyingRemote = false

    // Outbound wet stream (one local pen at a time).
    private var liveStrokeId: String?
    private var liveSeq: UInt32 = 0
    private var wetBuffer: [WetPoint] = []
    private var flushTask: Task<Void, Never>?
    /// Set at pen-up; `commit` consumes it so the committed stroke keeps the
    /// id receivers already saw on the wet channel.
    private var pendingCommitId: String?

    // Inbound wet overlays, keyed by remote stroke id.
    private var remoteWet: [String: RemoteWetStroke] = [:]

    init(session: NoteSession, sketchId: String) {
        self.session = session
        self.sketchId = sketchId
    }

    func attach(_ canvas: PKCanvasView, overlay: UIView) {
        self.canvas = canvas
        self.overlay = overlay
        // The model outlives its canvas (cached per note); a reattach gets a
        // fresh blank PKCanvasView. Stale side-tables would make
        // refreshFromCrdt skip repainting (ids already "match") and make
        // drawingChanged read every CRDT stroke as a local erase.
        ids = []
        keyToId = [:]
        remoteWet = [:]
        refreshFromCrdt()
    }

    func penState(down: Bool) {
        penDown = down
        if !down, pendingRefresh {
            pendingRefresh = false
            refreshFromCrdt()
        }
    }

    // MARK: outbound wet stream

    /// Pen-down with an inking tool: open the wet stream.
    func penBegan(at point: CGPoint, force: CGFloat) {
        guard let canvas, let ink = canvas.tool as? PKInkingTool else { return }
        liveSeq = 0
        wetBuffer = []
        liveStrokeId = try? session.beginStroke(
            sketch: sketchId,
            tool: StrokeCodec.tool(ink.inkType),
            color: StrokeCodec.pack(ink.color),
            baseWidth: Float(ink.width))
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
        guard liveStrokeId != nil else { return }
        for (point, force) in samples { buffer(point, force: force) }
    }

    /// Pen-up/cancel: flush the tail and hand the id to the upcoming commit.
    /// `finishStroke` (in `commit`) sends the wet End frame.
    func penEnded(cancelled: Bool) {
        flushTask?.cancel()
        flushTask = nil
        flushWet()
        pendingCommitId = cancelled ? nil : liveStrokeId
        liveStrokeId = nil
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

    // MARK: inbound wet overlay

    func remoteWetBegin(stroke: String, tool: Tool, color: UInt32, baseWidth: Float) {
        guard let overlay else { return }
        remoteWet[stroke]?.layer.removeFromSuperlayer()
        let wet = RemoteWetStroke(color: StrokeCodec.unpack(color), width: CGFloat(baseWidth))
        overlay.layer.addSublayer(wet.layer)
        remoteWet[stroke] = wet
    }

    func remoteWetPoints(stroke: String, points: [WetPoint]) {
        wetRecv += 1
        guard let wet = remoteWet[stroke] else { return }
        wet.append(points)
    }

    func remoteWetEnd(stroke: String) {
        // Keep the overlay until the committed stroke lands (strokesChanged →
        // refresh) so ink never blinks out; drop it now only as a fallback
        // when no commit ever arrives.
        guard let wet = remoteWet.removeValue(forKey: stroke) else { return }
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(2))
            wet.layer.removeFromSuperlayer()
        }
    }

    private func clearRemoteOverlays() {
        for wet in remoteWet.values { wet.layer.removeFromSuperlayer() }
        remoteWet = [:]
        overlay?.layer.sublayers?.forEach { $0.removeFromSuperlayer() }
    }

    /// Local mutation (draw or erase): diff drawing vs side-table.
    func drawingChanged() {
        guard !applyingRemote, let canvas else { return }
        let strokes = canvas.drawing.strokes
        var newIds: [String] = []
        var seen = Set<Date>()
        for stroke in strokes {
            let key = stroke.path.creationDate
            seen.insert(key)
            if let id = keyToId[key] {
                newIds.append(id)
            } else if let id = commit(stroke) {
                newIds.append(id)
            }
        }
        for (key, id) in keyToId where !seen.contains(key) {
            try? session.removeStroke(sketch: sketchId, stroke: id)
        }
        ids = newIds
        rebuildKeys(strokes)
        strokeCount = strokes.count
        grow(canvas)
    }

    func remoteChanged() {
        if penDown {
            pendingRefresh = true
        } else {
            refreshFromCrdt()
        }
    }

    /// Erase helper for the toolbar (and UI tests): removing via a drawing
    /// mutation exercises the same diff path PencilKit's eraser uses.
    func eraseLast() {
        guard let canvas, !canvas.drawing.strokes.isEmpty else { return }
        var drawing = canvas.drawing
        drawing.strokes.removeLast()
        canvas.drawing = drawing
    }

    private func commit(_ stroke: PKStroke) -> String? {
        // Streamed stroke: reuse the id the wet channel announced so
        // receivers swap their overlay for this committed stroke.
        if let id = pendingCommitId {
            pendingCommitId = nil
            try? session.finishStroke(sketch: sketchId, stroke: StrokeCodec.encode(stroke, id: id))
            return id
        }
        let encodedPreview = StrokeCodec.encode(stroke, id: "")
        guard
            let id = try? session.beginStroke(
                sketch: sketchId,
                tool: encodedPreview.tool,
                color: encodedPreview.color,
                baseWidth: encodedPreview.baseWidth)
        else { return nil }
        try? session.finishStroke(sketch: sketchId, stroke: StrokeCodec.encode(stroke, id: id))
        return id
    }

    private func refreshFromCrdt() {
        guard let canvas, let crdt = try? session.strokes(sketch: sketchId) else { return }
        clearRemoteOverlays()
        if crdt.map(\.id) == ids {
            strokeCount = ids.count
            return
        }
        let strokes = crdt.map(StrokeCodec.decode)
        applyingRemote = true
        canvas.drawing = PKDrawing(strokes: strokes)
        applyingRemote = false
        ids = crdt.map(\.id)
        rebuildKeys(strokes)
        strokeCount = ids.count
        grow(canvas)
    }

    private func rebuildKeys(_ strokes: [PKStroke]) {
        keyToId = Dictionary(
            zip(strokes.map { $0.path.creationDate }, ids),
            uniquingKeysWith: { first, _ in first })
    }

    /// Infinite canvas v1: grow content down/right only.
    private func grow(_ canvas: PKCanvasView) {
        let drawn = canvas.drawing.bounds
        let margin: CGFloat = 400
        let needed = CGSize(
            width: max(canvas.bounds.width, drawn.maxX + margin),
            height: max(canvas.bounds.height, drawn.maxY + margin))
        if canvas.contentSize.width < needed.width || canvas.contentSize.height < needed.height {
            canvas.contentSize = needed
        }
        overlay?.frame = CGRect(origin: .zero, size: canvas.contentSize)
    }
}

/// One in-flight remote stroke rendered as a polyline. Constant width
/// (remote wet points carry no per-point size yet); the committed stroke
/// replaces it with the real pressure-varying ink.
@MainActor
final class RemoteWetStroke {
    let layer = CAShapeLayer()
    private let path = UIBezierPath()
    private var started = false

    init(color: UIColor, width: CGFloat) {
        layer.strokeColor = color.cgColor
        layer.fillColor = nil
        layer.lineWidth = max(1, width)
        layer.lineCap = .round
        layer.lineJoin = .round
    }

    func append(_ points: [WetPoint]) {
        for p in points {
            let point = CGPoint(x: CGFloat(p.x), y: CGFloat(p.y))
            if started {
                path.addLine(to: point)
            } else {
                path.move(to: point)
                started = true
            }
        }
        layer.path = path.cgPath
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

        // Remote wet ink draws on an overlay that scrolls with the content
        // (a scroll view's subviews live in content coordinates).
        let overlay = UIView(frame: CGRect(origin: .zero, size: canvas.contentSize))
        overlay.isUserInteractionEnabled = false
        canvas.addSubview(overlay)

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

        model.attach(canvas, overlay: overlay)
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
