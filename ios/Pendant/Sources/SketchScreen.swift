// Full-screen sketch canvas with its own input pipeline and renderer; no
// PencilKit canvas. Raw touches (coalesced + predicted) feed the core's
// `BrushModeler`, which emits the stroke's points with their rendered
// width; `InkRenderer` draws those points as the core's lyon mesh in Metal.
// The live stroke, the committed stroke and every remote copy are the same
// geometry, so nothing changes on screen at pen-up — the reason PencilKit
// went (docs/plans/ink-renderer.md).
//
// View stack: a UIScrollView owns finger pan/zoom/inertia (its content view
// is an empty rect the size of the canvas); the Metal view sits above it,
// pinned to the screen, and reads the scroll state into its viewport each
// frame. A gesture recognizer on the scroll view captures pen touches.
// PencilKit survives only as the tool picker.
//
// Local strokes stream their stored points onto the ephemeral channel
// while drawn; pen-up commits `modeler.finish()` to the CRDT under the wet
// id, so receivers swap their provisional ink for identical committed ink.
// Pencil force and tilt arrive as estimates first: updates patch the
// modeler's points, and a commit waits up to `settleTimeout` for the
// updates its points still expect (the ink stays on screen meanwhile).
// Draw-and-hold: a pen still for `holdDelay` asks the core to recognise
// the stroke so far; a hit previews the snapped outline (with a haptic)
// and pen-up commits a shape element under the same wet id instead of a
// stroke. Dragging on after the snap resizes the shape from the point the
// pen held (the core's `resizeShape`), so the snap is never lost.
// Erasing: the eraser tool's samples hit-test whole elements in the core.
// Remote elements: wet batches render through `pointsMesh`; strokesChanged
// diffs the CRDT into meshes — deferred while a local pen is down.

import MetalKit
import PencilKit
import PendantCore
import SwiftUI
import UIKit

/// Wet batches leave the pen at this cadence (plan: 60ms ≈ 4-8 samples).
private let wetFlushInterval: Duration = .milliseconds(60)
/// The pen must stay within this many screen points for `holdDelay` to
/// count as a draw-and-hold.
private let holdRadius: CGFloat = 3
private let holdDelay: Duration = .milliseconds(500)
/// Backpressure cap: ~2s of 240Hz samples. Overflow drops the oldest —
/// wet ink is lossy-tolerant, the committed stroke is not built from it.
private let wetBufferCap = 480
/// Provisional ink lingers this long after End if the commit never lands
/// (same as the desktop).
private let wetLinger: Duration = .seconds(5)
/// After pen-up, wait at most this long for estimated force/tilt updates
/// before committing; a committed stroke is never edited.
private let settleTimeout: Duration = .milliseconds(200)
/// Pencil force in Apple's units is 1 at average pressure and can reach
/// about 4; full ink width sits at twice average so hard pressure has room
/// to show. Fingers report no force and get the nominal 0.5.
private let forceFullScale: CGFloat = 2
/// Canvas grows in steps this far beyond the ink (infinite canvas v1:
/// down/right only).
private let canvasMargin: CGFloat = 400
private let minZoom: CGFloat = 0.5
private let maxZoom: CGFloat = 8

/// Which touches ink. The Pencil only on device (fingers pan and zoom);
/// fingers too on the simulator and under `-anyInput 1`, so UI tests can
/// draw with drags. `-anyInput 0` forces pencil-only anywhere.
enum InputPolicy {
    case pencilOnly
    case anyInput

    static var launch: InputPolicy {
        let defaults = UserDefaults.standard
        if defaults.object(forKey: "anyInput") != nil {
            return defaults.bool(forKey: "anyInput") ? .anyInput : .pencilOnly
        }
        #if targetEnvironment(simulator)
            return .anyInput
        #else
            return .pencilOnly
        #endif
    }

    var touchTypes: [NSNumber] {
        switch self {
        case .pencilOnly: [NSNumber(value: UITouch.TouchType.pencil.rawValue)]
        case .anyInput:
            [
                NSNumber(value: UITouch.TouchType.pencil.rawValue),
                NSNumber(value: UITouch.TouchType.direct.rawValue),
            ]
        }
    }
}

extension RawSample {
    /// One touch as the modeler sees it, in `view`'s (canvas) coordinates.
    /// Tilt is captured for every pencil touch (barrel roll is Apple Pencil
    /// Pro only, iOS 17.5+, 0 otherwise). Force and tilt may still be
    /// estimates: `estimationId` names the touch so the update can be
    /// patched in through `BrushModeler.update`.
    init(_ touch: UITouch, in view: UIView) {
        let location = touch.location(in: view)
        let pencil = touch.type == .pencil
        var tilt: Tilt?
        if pencil {
            var roll: CGFloat = 0
            if #available(iOS 17.5, *) { roll = touch.rollAngle }
            tilt = Tilt(
                azimuth: Float(touch.azimuthAngle(in: view)),
                altitude: Float(touch.altitudeAngle),
                roll: Float(roll))
        }
        let expecting = touch.estimatedPropertiesExpectingUpdates
        self.init(
            x: Float(location.x), y: Float(location.y),
            force: pencil ? Float(min(touch.force / forceFullScale, 1)) : 0.5,
            tMs: touch.timestamp * 1000,
            tilt: tilt,
            estimationId: touch.estimationUpdateIndex?.uint32Value,
            expectsUpdate: !expecting.intersection([.force, .azimuth, .altitude]).isEmpty)
    }
}

/// A local stroke: in progress, or past pen-up and settling.
@MainActor
private struct LiveStroke {
    let id: String
    let modeler: BrushModeler
    let selection: BrushSelection
    var brush: BrushRef { selection.brush }
    var color: UInt32 { selection.color }
    var seq: UInt32 = 0
    /// Stored points not yet streamed.
    var wetBuffer: [StrokePoint] = []
    /// Points streamed so far; the commit's wet tail starts here.
    var sentPoints = 0
    /// Where the pen last came to rest; the hold timer runs from there.
    var holdAnchor: RawSample?
    var holdTask: Task<Void, Never>?
    /// The shape this stroke will commit as, once a hold recognised one.
    var snap: Recognition?
    /// Where the pen was when the shape snapped; dragging away from here
    /// resizes the snapped shape instead of discarding it.
    var snapPen: RawSample?
    /// The snapped shape after the drag so far.
    var resized: PendantCore.Shape?
    /// The shape to preview and commit.
    var shape: PendantCore.Shape? { resized ?? snap?.shape }
    /// Raw samples kept for `-recordStrokes 1`, patched as estimates land.
    var recording: [RawSample]?
    /// Estimated-property bookkeeping for the `est:` commit log.
    var estPushed = 0
    var estUpdated = 0
    var estLate = 0
    var maxLateMs = 0.0
    var endedAt: Date?
    var settleTask: Task<Void, Never>?
}

/// Writes raw pen samples to Documents for the core's shape corpus
/// (`crates/pendant-core/tests/corpus/shapes`), one file per stroke in the
/// replay format. Enabled by the `-recordStrokes 1` launch argument; the
/// app shares Documents with Files so the recordings can be copied out.
private enum StrokeRecorder {
    static let enabled = UserDefaults.standard.bool(forKey: "recordStrokes")

    /// Format v2: header lines, then `x y force t_ms azimuth altitude roll
    /// est` per sample (`nan` tilt without a pencil; `est` 1 when the row
    /// was still an estimate at commit). Rows are written after the stroke
    /// settled, so updates are already patched in.
    static func save(_ samples: [RawSample], selection: BrushSelection, zoom: CGFloat) {
        guard enabled, !samples.isEmpty,
              let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first
        else { return }
        let dir = documents.appendingPathComponent("strokes", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let stamp = Int64(Date().timeIntervalSince1970 * 1000)
        let device = UIDevice.current
        var text = "# pendant-stroke v2\n# expect: none\n"
        text += "# tool: \(selection.brush.tool) size: \(selection.brush.baseWidth)"
        text += String(format: " color: %08x brush: preset\n", selection.color)
        text += "# device: \(device.model) ios: \(device.systemVersion) force: half-average"
        text += String(format: " zoom: %.2f\n", zoom)
        text += "# columns: x y force t_ms azimuth altitude roll est\n"
        for s in samples {
            let tilt = s.tilt.map { String(format: "%.3f %.3f %.3f", $0.azimuth, $0.altitude, $0.roll) }
                ?? "nan nan nan"
            text += String(format: "%.2f %.2f %.3f %.1f ", s.x, s.y, s.force, s.tMs)
            text += tilt + (s.expectsUpdate ? " 1\n" : " 0\n")
        }
        try? text.write(to: dir.appendingPathComponent("stroke-\(stamp).txt"), atomically: true, encoding: .utf8)
        // Also to the console, so an attached `devicectl --console` captures
        // the stroke without a trip through Files.
        NSLog("recorded %d samples to strokes/stroke-%lld.txt\n--- begin stroke-%lld.txt ---\n%@--- end ---", samples.count, stamp, stamp, text)
    }
}

@Observable @MainActor
final class SketchModel {
    let session: NoteSession
    let sketchId: String
    /// Stroke elements on screen.
    var strokeCount = 0
    /// Shape elements on screen.
    var shapeCount = 0
    /// Diagnostics surfaced in the status label (log capture on the sim is
    /// unreliable): outbound wet flushes and inbound wet batches this session.
    var wetSent = 0
    var wetRecv = 0
    /// Selected in the PencilKit tool picker; inking tools draw, the eraser
    /// erases, anything else is ignored.
    var tool: PKTool
    /// `-tool <pen|pencil|marker|monoline|fountain|crayon>` pins the tool
    /// and skips the picker (UI tests).
    let toolOverride: PKInkingTool?

    private weak var canvas: SketchCanvasView?
    private var renderer: InkRenderer? { canvas?.renderer }
    /// Committed element ids on screen, in CRDT order.
    private var ids: [String] = []
    /// The subset of `ids` that are shapes.
    private var shapeIds: Set<String> = []
    private var penDown = false
    private var pendingRefresh = false
    private var erasing = false
    private var live: LiveStroke?
    /// Pen-up strokes waiting for estimated-property updates, by id.
    private var settling: [String: LiveStroke] = [:]
    private var flushTask: Task<Void, Never>?
    private var selfTestDone = false

    init(session: NoteSession, sketchId: String) {
        self.session = session
        self.sketchId = sketchId
        let name = UserDefaults.standard.string(forKey: "tool") ?? ""
        let override = StrokeCodec.selection(named: name).map { _ in
            PKInkingTool(Self.inkType(named: name), color: .black, width: 10)
        }
        toolOverride = override
        tool = override ?? PKInkingTool(.pen, color: .black, width: 10)
    }

    private static func inkType(named name: String) -> PKInkingTool.InkType {
        switch name {
        case "pencil": .pencil
        case "marker": .marker
        case "monoline": .monoline
        case "fountain", "fountainPen": .fountainPen
        case "crayon": .crayon
        default: .pen
        }
    }

    func attach(_ canvas: SketchCanvasView) {
        self.canvas = canvas
        // The model outlives its canvas (cached per note); a reattach gets a
        // fresh canvas + renderer, so forget what the old one showed.
        ids = []
        shapeIds = []
        canvas.renderer.removeAll()
        refreshFromCrdt()
        if UserDefaults.standard.bool(forKey: "figureEight"), !selfTestDone {
            selfTestDone = true
            drawFigureEight()
        }
    }

    /// `-figureEight 1`: commit one stroke that crosses itself, for the
    /// marker self-overlap UI test (XCUITest drags are straight lines).
    /// Lemniscate centred at (300, 300), crossing there, arm tip at (440, 300).
    private func drawFigureEight() {
        guard let ink = tool as? PKInkingTool else { return }
        let n = 240
        let samples = (0...n).map { i -> RawSample in
            let t = Float(i) / Float(n) * 2 * .pi
            return RawSample(
                x: 300 + 140 * sin(t), y: 300 + 140 * sin(t) * cos(t), force: 0.5,
                tMs: Double(i) * 8, tilt: nil, estimationId: nil, expectsUpdate: false)
        }
        beginStroke(StrokeCodec.selection(inking: ink), at: samples[0])
        penMoved(coalesced: Array(samples.dropFirst()), predicted: [])
        penEnded(cancelled: false)
    }

    private func updateCounts() {
        shapeCount = shapeIds.count
        strokeCount = ids.count - shapeCount
    }

    // MARK: pen input

    func penBegan(_ sample: RawSample) {
        penDown = true
        if let eraser = tool as? PKEraserTool {
            erasing = true
            erase(at: sample, radius: Self.eraserRadius(eraser))
            return
        }
        guard let ink = tool as? PKInkingTool else { return }
        beginStroke(StrokeCodec.selection(inking: ink), at: sample)
        flushTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: wetFlushInterval)
                self?.flushWet()
            }
        }
    }

    private func beginStroke(_ selection: BrushSelection, at sample: RawSample) {
        guard
            let id = try? session.beginStroke(
                sketch: sketchId, tool: selection.brush.tool, color: selection.color,
                baseWidth: selection.brush.baseWidth)
        else { return }
        var stroke = LiveStroke(
            id: id, modeler: BrushModeler(tool: selection.brush.tool, size: selection.brush.baseWidth),
            selection: selection)
        stroke.wetBuffer = stroke.modeler.push(samples: [sample])
        if sample.expectsUpdate { stroke.estPushed += 1 }
        if StrokeRecorder.enabled { stroke.recording = [sample] }
        live = stroke
        armHold(at: sample)
        showLive(predicted: [])
    }

    /// `coalesced` are the real samples since the last event; `predicted`
    /// is Apple's guess at the next few, drawn as a tail and discarded on
    /// the next event.
    func penMoved(coalesced: [RawSample], predicted: [RawSample]) {
        if erasing, let eraser = tool as? PKEraserTool {
            let radius = Self.eraserRadius(eraser)
            for sample in coalesced { erase(at: sample, radius: radius) }
            return
        }
        guard live != nil else { return }
        let emitted = live!.modeler.push(samples: coalesced)
        live!.wetBuffer.append(contentsOf: emitted)
        if live!.wetBuffer.count > wetBufferCap {
            live!.wetBuffer.removeFirst(live!.wetBuffer.count - wetBufferCap)
        }
        live!.estPushed += coalesced.filter(\.expectsUpdate).count
        live!.recording?.append(contentsOf: coalesced)
        if let last = coalesced.last { armHold(at: last) }
        showLive(predicted: predicted)
    }

    /// UIKit revised the force or tilt of earlier touches: patch the
    /// modeler's points (live or settling) and redraw. A settling stroke
    /// commits as soon as its last expected update lands.
    func penEstimateUpdated(_ samples: [RawSample]) {
        for sample in samples {
            guard let id = sample.estimationId else { continue }
            if live != nil, live!.modeler.update(estimationId: id, force: sample.force, tilt: sample.tilt) {
                live!.estUpdated += 1
                Self.patchRecording(&live!, sample)
                showLive(predicted: [])
                continue
            }
            for key in settling.keys
            where settling[key]!.modeler.update(estimationId: id, force: sample.force, tilt: sample.tilt) {
                settling[key]!.estUpdated += 1
                settling[key]!.estLate += 1
                if let ended = settling[key]!.endedAt {
                    let late = Date().timeIntervalSince(ended) * 1000
                    settling[key]!.maxLateMs = max(settling[key]!.maxLateMs, late)
                }
                Self.patchRecording(&settling[key]!, sample)
                if settling[key]!.modeler.pendingEstimates().isEmpty { commitSettled(key) }
                break
            }
        }
    }

    private static func patchRecording(_ stroke: inout LiveStroke, _ sample: RawSample) {
        guard stroke.recording != nil,
              let i = stroke.recording!.lastIndex(where: { $0.estimationId == sample.estimationId })
        else { return }
        stroke.recording![i].force = sample.force
        stroke.recording![i].tilt = sample.tilt
        stroke.recording![i].expectsUpdate = false
    }

    // MARK: draw-and-hold

    /// Keep the hold timer running while the pen stays within `holdRadius`
    /// of where it came to rest; any larger move re-anchors and restarts
    /// the timer. Once a shape has snapped, moving resizes it instead.
    private func armHold(at sample: RawSample) {
        guard live != nil else { return }
        let zoom = Float(max(renderer?.viewport.zoom ?? 1, 0.01))
        let radius = Float(holdRadius) / zoom
        if let anchor = live!.holdAnchor,
           hypot(sample.x - anchor.x, sample.y - anchor.y) <= radius
        {
            return
        }
        if let snap = live!.snap, let from = live!.snapPen {
            live!.resized = resizeShape(
                shape: snap.shape, from: Point2(x: from.x, y: from.y),
                to: Point2(x: sample.x, y: sample.y))
            return
        }
        live!.holdTask?.cancel()
        live!.holdAnchor = sample
        let id = live!.id
        live!.holdTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: holdDelay)
            guard !Task.isCancelled else { return }
            self?.holdFired(stroke: id)
        }
    }

    private func holdFired(stroke id: String) {
        guard live?.id == id, live?.snap == nil else { return }
        // The pen may have wandered `holdRadius` screen points during the
        // hold; tell the recognizer that in canvas units, with slack.
        let zoom = Float(max(renderer?.viewport.zoom ?? 1, 0.01))
        let radius = 1.5 * Float(holdRadius) / zoom
        guard let rec = recognizeShape(points: live!.modeler.points(), holdRadius: radius) else {
            NSLog("hold: no shape recognised from %d points", live!.modeler.points().count)
            return
        }
        live!.snap = rec
        live!.snapPen = live!.holdAnchor
        live!.resized = nil
        NSLog("hold snapped to %@ (confidence %.2f)", String(describing: rec.shape), rec.confidence)
        showLive(predicted: [])
        snapHaptic()
    }

    private func snapHaptic() {
        guard let canvas else { return }
        if #available(iOS 17.5, *) {
            let generator = UICanvasFeedbackGenerator(view: canvas)
            generator.alignmentOccurred(at: CGPoint(x: canvas.bounds.midX, y: canvas.bounds.midY))
        } else {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
        }
    }

    /// Pen-up commits the modelled stroke under the wet id (`finishStroke`
    /// sends the wet End frame) — at once, or after the estimated updates
    /// its points still expect (≤ `settleTimeout`, ink kept on screen). A
    /// cancelled touch sends Cancel so receivers drop the provisional ink.
    func penEnded(cancelled: Bool) {
        penDown = false
        defer { flushPendingRefresh() }
        if erasing {
            erasing = false
            return
        }
        flushTask?.cancel()
        flushTask = nil
        flushWet()
        guard var stroke = live else { return }
        live = nil
        stroke.holdTask?.cancel()
        if cancelled {
            renderer?.clearLocal()
            try? session.cancelStroke(stroke: stroke.id)
            return
        }
        stroke.endedAt = Date()
        // A snapped shape ignores force; a stroke waits for its estimates.
        let pending = stroke.shape == nil ? stroke.modeler.pendingEstimates() : []
        if pending.isEmpty {
            renderer?.clearLocal()
            commit(stroke)
            return
        }
        renderer?.settleLocal(as: stroke.id)
        let id = stroke.id
        stroke.settleTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: settleTimeout)
            guard !Task.isCancelled else { return }
            self?.commitSettled(id)
        }
        settling[id] = stroke
    }

    private func commitSettled(_ id: String) {
        guard let stroke = settling.removeValue(forKey: id) else { return }
        stroke.settleTask?.cancel()
        commit(stroke)
    }

    private func commit(_ stroke: LiveStroke) {
        if let recording = stroke.recording {
            StrokeRecorder.save(
                recording, selection: stroke.selection, zoom: renderer?.viewport.zoom ?? 1)
        }
        let createdMs = UInt64(max(0, Date().timeIntervalSince1970 * 1000))
        let element: Element
        if let snapped = stroke.shape {
            // Same id as the wet stream: receivers swap ink for shape.
            let shape = ShapeElement(
                id: stroke.id, shape: snapped, tool: stroke.brush.tool, color: stroke.color,
                width: stroke.brush.baseWidth, start: nil, end: nil, createdMs: createdMs)
            try? session.finishShape(sketch: sketchId, shape: shape)
            element = .shape(shape)
            shapeIds.insert(stroke.id)
        } else {
            let points = stroke.modeler.finish()
            let tail = Array(points.dropFirst(min(stroke.sentPoints, points.count)))
            let committed = Stroke(
                id: stroke.id, tool: stroke.brush.tool, color: stroke.color,
                baseWidth: stroke.brush.baseWidth, kind: .polylineSample, points: points,
                createdMs: createdMs)
            try? session.finishStroke(sketch: sketchId, stroke: committed, tail: tail)
            element = .stroke(committed)
        }
        if stroke.estPushed > 0 {
            let waited = stroke.endedAt.map { Date().timeIntervalSince($0) * 1000 } ?? 0
            NSLog(
                "est: pushed=%d updated=%d late=%d maxLateMs=%.0f waitedMs=%.0f unresolved=%d",
                stroke.estPushed, stroke.estUpdated, stroke.estLate, stroke.maxLateMs, waited,
                stroke.modeler.pendingEstimates().count)
        }
        // `show` also drops the settling copy of the same id.
        renderer?.show(element, z: ids.count)
        ids.append(stroke.id)
        updateCounts()
        grow()
    }

    private func showLive(predicted: [RawSample]) {
        guard let stroke = live else { return }
        if let shape = stroke.shape {
            renderer?.setLocalShape(shape, brush: stroke.brush, color: stroke.color)
            return
        }
        let points = stroke.modeler.points() + stroke.modeler.predict(samples: predicted)
        renderer?.setLocal(points: points, brush: stroke.brush, color: stroke.color)
    }

    private func flushWet() {
        guard live != nil, !live!.wetBuffer.isEmpty else { return }
        live!.seq += 1
        wetSent += 1
        try? session.appendPoints(stroke: live!.id, seq: live!.seq, points: live!.wetBuffer)
        live!.wetBuffer = []
        live!.sentPoints = live!.modeler.points().count
    }

    private func flushPendingRefresh() {
        if pendingRefresh {
            pendingRefresh = false
            refreshFromCrdt()
        }
    }

    // MARK: eraser

    private func erase(at sample: RawSample, radius: Float) {
        guard
            let removed = try? session.eraseAt(
                sketch: sketchId, x: sample.x, y: sample.y, radius: radius),
            !removed.isEmpty
        else { return }
        let gone = Set(removed)
        for id in gone { renderer?.remove(id) }
        ids.removeAll { gone.contains($0) }
        shapeIds.subtract(gone)
        updateCounts()
    }

    private static func eraserRadius(_ eraser: PKEraserTool) -> Float {
        if #available(iOS 16.4, *) {
            return Float(max(4, eraser.width / 2))
        }
        return 12
    }

    // MARK: inbound wet ink

    func remoteWetBegin(stroke: String, tool: Tool, color: UInt32, baseWidth: Float) {
        renderer?.wetBegin(stroke, brush: BrushRef(tool: tool, baseWidth: baseWidth), color: color)
    }

    func remoteWetPoints(stroke: String, points: [StrokePoint]) {
        wetRecv += 1
        renderer?.wetAppend(stroke, points)
    }

    /// Sender says no stroke is coming: drop the provisional ink right away.
    func remoteWetCancel(stroke: String) {
        renderer?.wetRemove(stroke)
    }

    func remoteWetEnd(stroke: String) {
        // The sender's tail arrived as points; finish the mesh with its end
        // taper. Keep the wet ink until the committed stroke lands
        // (strokesChanged → refresh) so ink never blinks out; drop it only
        // as a fallback when no commit ever arrives.
        renderer?.wetEnd(stroke, tail: [])
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: wetLinger)
            self?.renderer?.wetRemove(stroke)
        }
    }

    // MARK: CRDT → renderer

    func remoteChanged() {
        if penDown {
            pendingRefresh = true
        } else {
            refreshFromCrdt()
        }
    }

    /// Erase helper for the toolbar (and UI tests): drops the newest element
    /// through the same CRDT path the eraser uses.
    func eraseLast() {
        guard let last = ids.last else { return }
        try? session.removeElement(sketch: sketchId, element: last)
        renderer?.remove(last)
        ids.removeLast()
        shapeIds.remove(last)
        updateCounts()
    }

    private func refreshFromCrdt() {
        guard let renderer, let crdt = try? session.elements(sketch: sketchId) else { return }
        let newIds = crdt.map(\.id)
        if newIds != ids {
            let keep = Set(newIds)
            for id in ids where !keep.contains(id) { renderer.remove(id) }
            for (z, element) in crdt.enumerated() { renderer.show(element, z: z) }
            ids = newIds
        }
        shapeIds = Set(crdt.filter(\.isShape).map(\.id))
        // A committed element replaces its wet ink.
        for id in newIds where renderer.hasWet(id) { renderer.wetRemove(id) }
        updateCounts()
        grow()
    }

    /// Infinite canvas v1: grow content down/right only.
    private func grow() {
        guard let canvas, let renderer else { return }
        let drawn = renderer.inkBounds
        guard !drawn.isNull else { return }
        canvas.ensureCanvas(
            covers: CGSize(width: drawn.maxX + canvasMargin, height: drawn.maxY + canvasMargin))
    }
}

/// Captures pen (or, under `.anyInput`, finger) touches with the full
/// `UIEvent`, which is where coalesced and predicted touches live. Runs
/// alongside the scroll view's own recognizers; tracks one touch and
/// cancels the stroke when a second one lands (a pinch, not ink).
@MainActor
final class PenGestureRecognizer: UIGestureRecognizer, UIGestureRecognizerDelegate {
    enum Phase {
        case began(RawSample)
        case moved(coalesced: [RawSample], predicted: [RawSample])
        /// Revised force/tilt for earlier samples, keyed by `estimationId`.
        case estimateUpdated([RawSample])
        case ended
        case cancelled
    }

    var onPhase: ((Phase) -> Void)?
    /// Touch locations are taken in this view (the zoomable canvas).
    weak var canvasSpace: UIView?
    private var tracked: UITouch?

    init(policy: InputPolicy) {
        super.init(target: nil, action: nil)
        allowedTouchTypes = policy.touchTypes
        cancelsTouchesInView = false
        delaysTouchesBegan = false
        delaysTouchesEnded = false
        delegate = self
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        if tracked != nil {
            // A second finger: this is a pinch, not a stroke.
            finish(.cancelled)
            state = .cancelled
            return
        }
        guard let touch = touches.first, let space = canvasSpace else { return }
        tracked = touch
        onPhase?(.began(RawSample(touch, in: space)))
        state = .began
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        guard let touch = tracked, touches.contains(touch), let space = canvasSpace else { return }
        let coalesced = (event.coalescedTouches(for: touch) ?? [touch]).map {
            RawSample($0, in: space)
        }
        let predicted = (event.predictedTouches(for: touch) ?? []).map { RawSample($0, in: space) }
        onPhase?(.moved(coalesced: coalesced, predicted: predicted))
        state = .changed
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        guard let touch = tracked, touches.contains(touch) else { return }
        finish(.ended)
        state = .ended
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        guard let touch = tracked, touches.contains(touch) else { return }
        finish(.cancelled)
        state = .cancelled
    }

    /// Arrives for pencil touches while and after the stroke, until every
    /// estimated property settled; not gated on `tracked`.
    override func touchesEstimatedPropertiesUpdated(_ touches: Set<UITouch>) {
        guard let space = canvasSpace else { return }
        onPhase?(.estimateUpdated(touches.map { RawSample($0, in: space) }))
    }

    override func reset() {
        tracked = nil
    }

    private func finish(_ phase: Phase) {
        tracked = nil
        onPhase?(phase)
    }

    nonisolated func gestureRecognizer(
        _ gestureRecognizer: UIGestureRecognizer,
        shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer
    ) -> Bool {
        true
    }
}

/// The sketch surface: scroll view (pan/zoom/inertia) under a pinned Metal
/// view. First responder so the PencilKit tool picker attaches to it.
@MainActor
final class SketchCanvasView: UIView, UIScrollViewDelegate {
    let scroll = UIScrollView()
    /// Zoomable, empty; its frame is the canvas. Touch locations are read
    /// in this view so they are canvas coordinates at any zoom.
    let content = UIView()
    let metal: MTKView
    let renderer: InkRenderer
    let pen: PenGestureRecognizer

    init?(policy: InputPolicy) {
        metal = MTKView(frame: .zero, device: MTLCreateSystemDefaultDevice())
        guard let renderer = InkRenderer(view: metal) else { return nil }
        self.renderer = renderer
        pen = PenGestureRecognizer(policy: policy)
        super.init(frame: .zero)

        isAccessibilityElement = true
        accessibilityIdentifier = "sketchCanvas"

        scroll.delegate = self
        scroll.minimumZoomScale = minZoom
        scroll.maximumZoomScale = maxZoom
        scroll.bouncesZoom = true
        scroll.contentInsetAdjustmentBehavior = .never
        scroll.showsVerticalScrollIndicator = false
        scroll.showsHorizontalScrollIndicator = false
        scroll.backgroundColor = .clear
        let direct = [NSNumber(value: UITouch.TouchType.direct.rawValue)]
        switch policy {
        case .pencilOnly:
            scroll.panGestureRecognizer.allowedTouchTypes = direct
            scroll.pinchGestureRecognizer?.allowedTouchTypes = direct
        case .anyInput:
            // One finger inks; two pan or pinch.
            scroll.panGestureRecognizer.minimumNumberOfTouches = 2
        }
        content.isUserInteractionEnabled = false
        scroll.addSubview(content)
        addSubview(scroll)

        metal.delegate = renderer
        metal.isPaused = true
        metal.enableSetNeedsDisplay = true
        metal.isUserInteractionEnabled = false
        metal.isOpaque = true
        addSubview(metal)
        applyBackground()

        pen.canvasSpace = content
        scroll.addGestureRecognizer(pen)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var canBecomeFirstResponder: Bool { true }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        applyDrawableScale()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        scroll.frame = bounds
        metal.frame = bounds
        applyDrawableScale()
        ensureCanvas(covers: bounds.size)
        pushViewport()
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

    /// Grow the canvas (never shrink) so `size` fits at zoom 1.
    func ensureCanvas(covers size: CGSize) {
        let current = content.bounds.size
        let needed = CGSize(width: max(current.width, size.width), height: max(current.height, size.height))
        guard needed != current else { return }
        content.frame = CGRect(origin: .zero, size: needed)
        scroll.contentSize = CGSize(
            width: needed.width * scroll.zoomScale, height: needed.height * scroll.zoomScale)
    }

    private func pushViewport() {
        renderer.viewport = Viewport(
            zoom: scroll.zoomScale, offset: scroll.contentOffset, size: bounds.size)
    }

    private func applyBackground() {
        metal.clearColor = InkRenderer.clearColor(for: .systemBackground, trait: traitCollection)
        renderer.needsDisplay()
    }

    override func traitCollectionDidChange(_ previous: UITraitCollection?) {
        super.traitCollectionDidChange(previous)
        if traitCollection.hasDifferentColorAppearance(comparedTo: previous) { applyBackground() }
    }

    // MARK: UIScrollViewDelegate

    nonisolated func viewForZooming(in scrollView: UIScrollView) -> UIView? {
        MainActor.assumeIsolated { content }
    }

    nonisolated func scrollViewDidScroll(_ scrollView: UIScrollView) {
        MainActor.assumeIsolated { pushViewport() }
    }

    nonisolated func scrollViewDidZoom(_ scrollView: UIScrollView) {
        MainActor.assumeIsolated { pushViewport() }
    }
}

struct SketchScreen: View {
    let model: SketchModel
    let done: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("strokes=\(model.strokeCount) shapes=\(model.shapeCount) wetSent=\(model.wetSent) wetRecv=\(model.wetRecv)")
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

    func makeUIView(context: Context) -> UIView {
        guard let canvas = SketchCanvasView(policy: .launch) else {
            // No Metal device: nothing to draw with. Leave a labelled blank so
            // the screen still opens and closes.
            let fallback = UILabel()
            fallback.text = "Metal is unavailable on this device"
            fallback.textAlignment = .center
            fallback.accessibilityIdentifier = "sketchCanvas"
            return fallback
        }
        let model = model
        canvas.pen.onPhase = { phase in
            switch phase {
            case .began(let sample): model.penBegan(sample)
            case .moved(let coalesced, let predicted):
                model.penMoved(coalesced: coalesced, predicted: predicted)
            case .estimateUpdated(let samples): model.penEstimateUpdated(samples)
            case .ended: model.penEnded(cancelled: false)
            case .cancelled: model.penEnded(cancelled: true)
            }
        }

        // PencilKit's picker works with any first responder; it hands us
        // the selected tool through the observer. `-tool` pins the tool
        // instead (UI tests).
        if model.toolOverride == nil {
            let picker = Self.makePicker()
            picker.setVisible(true, forFirstResponder: canvas)
            picker.addObserver(context.coordinator)
            context.coordinator.picker = picker
            canvas.becomeFirstResponder()
            if let selected = context.coordinator.selectedTool { model.tool = selected }
        }

        model.attach(canvas)
        return canvas
    }

    func updateUIView(_ view: UIView, context: Context) {}

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
        let model: SketchModel
        var picker: PKToolPicker?

        init(model: SketchModel) { self.model = model }

        var selectedTool: PKTool? {
            guard let picker else { return nil }
            if #available(iOS 18.0, *) {
                switch picker.selectedToolItem {
                case let inking as PKToolPickerInkingItem: return inking.inkingTool
                case let eraser as PKToolPickerEraserItem: return eraser.eraserTool
                default: return nil
                }
            }
            return picker.selectedTool
        }

        nonisolated func toolPickerSelectedToolDidChange(_ toolPicker: PKToolPicker) {
            MainActor.assumeIsolated {
                if let selected = selectedTool { model.tool = selected }
            }
        }

        @available(iOS 18.0, *)
        nonisolated func toolPickerSelectedToolItemDidChange(_ toolPicker: PKToolPicker) {
            MainActor.assumeIsolated {
                if let selected = selectedTool { model.tool = selected }
            }
        }
    }
}
