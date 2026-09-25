// Ink on the note page: the input pipeline behind `NoteCanvasView`.
// Raw touches (coalesced + predicted) feed the core's `BrushModeler`,
// which emits the stroke's points with their rendered width; `InkRenderer`
// draws those points as the core's lyon mesh in Metal. The live stroke,
// the committed stroke and every remote copy are the same geometry, so
// nothing changes on screen at pen-up (docs/plans/ink-renderer.md).
//
// Every element is anchored to the source line it was drawn on: pen-down
// asks the layout which line is under the pen, takes a CRDT anchor for
// it, and every point of the stroke is stored relative to that line's
// origin (its left edge, the top of its first fragment). When the text
// changes the layout reports, the anchors are re-resolved and the ink is
// re-placed; a stroke in progress moves with its line too and the pen
// keeps drawing under the tip.
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
// Erasing: the eraser's samples hit-test whole elements in the core, each
// probe expressed in that element's own space.
// Remote elements: wet batches render through `pointsMesh`; pageChanged
// diffs the CRDT into meshes — deferred while a local pen is down.
//
// Two layers share the renderer. The overlay (the `page` list) is the
// line-anchored ink above. An inline sketch is a legacy `sketches`
// container shown as a box in the text flow: a pen-down inside a box
// draws into that container in the box's own space (origin = the box
// corner inset by the padding), commits with the sketch-keyed calls and
// streams its wet ink under `WetInk::Begin`. Inline elements sit below
// the overlay in z. A box appears and disappears with its embed line;
// the layout reports the boxes and `syncInline` loads, moves or drops
// the sketch's elements to match.

import Observation
import PencilKit
import KrabinkCore
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
/// Alpha of the hover preview dab relative to the ink's own.
private let hoverAlpha: Float = 0.35
/// Peers see the pen as a pointer: send its position at most this often
/// (hover events arrive at 120 Hz+) and only once it moved this far on
/// screen, unless the pen went down or up.
private let pointerInterval: CFAbsoluteTime = 1.0 / 30
private let pointerMinMove: CGFloat = 1.5

/// Which layer an element lives in and what it is placed by.
enum Placement: Equatable {
    /// Overlay: anchored to a source line.
    case line(anchor: Data)
    /// Inline: inside the box of this sketch.
    case sketch(String)
}

/// A local stroke: in progress, or past pen-up and settling. Its points
/// are in its layer's space (page point − `origin`).
@MainActor
private struct LiveStroke {
    let id: String
    let modeler: BrushModeler
    let selection: BrushSelection
    let placement: Placement
    /// The line under the pen at pen-down (the stroke's line for the
    /// overlay; the embed line for an inline stroke, for the pointer).
    let anchor: Data
    /// Scalar index of that line's first char at pen-down.
    let lineStart: Int
    /// Where the stroke's space is on the page right now.
    var origin: CGPoint
    var brush: BrushRef { selection.brush }
    var color: UInt32 { selection.color }
    var seq: UInt32 = 0
    /// Stored points not yet streamed.
    var wetBuffer: [StrokePoint] = []
    /// Points streamed so far; the commit's wet tail starts here.
    var sentPoints = 0
    /// Where the pen last came to rest (anchor space); the hold timer
    /// runs from there.
    var holdAnchor: RawSample?
    var holdTask: Task<Void, Never>?
    /// The shape this stroke will commit as, once a hold recognised one.
    var snap: Recognition?
    /// Where the pen was when the shape snapped; dragging away from here
    /// resizes the snapped shape instead of discarding it.
    var snapPen: RawSample?
    /// The snapped shape after the drag so far.
    var resized: KrabinkCore.Shape?
    /// The shape to preview and commit.
    var shape: KrabinkCore.Shape? { resized ?? snap?.shape }
    /// Raw samples kept for `-recordStrokes 1`, patched as estimates land.
    var recording: [RawSample]?
    /// Estimated-property bookkeeping for the `est:` commit log.
    var estPushed = 0
    var estUpdated = 0
    var estLate = 0
    var maxLateMs = 0.0
    var endedAt: Date?
    var settleTask: Task<Void, Never>?
    /// Live redraws this stroke and the slowest one, ms.
    var redraws = 0
    var maxRedrawMs = 0.0
}

@Observable @MainActor
final class PageInkModel {
    let session: NoteSession
    /// Stroke elements on screen.
    var strokeCount = 0
    /// Committed strokes drawn with a custom brush (status label, UI tests).
    var customCount = 0
    /// Shape elements on screen.
    var shapeCount = 0
    /// Diagnostics surfaced in the status label (log capture on the sim is
    /// unreliable): outbound wet flushes and inbound wet batches this session.
    var wetSent = 0
    var wetRecv = 0
    /// Points whose estimated force or tilt was revised before commit.
    var estUpdated = 0
    /// Page y of the oldest element's line (status label: UI tests watch
    /// it move when text above the ink changes).
    var originY: CGFloat = 0
    /// Inline elements on screen, over every box.
    var inlineCount = 0
    /// Page y of the first inline box (status label).
    var boxY: CGFloat = 0
    /// A sketch's elements changed here or remotely: its box height may
    /// have moved. The canvas re-measures.
    @ObservationIgnored var onSketchChanged: ((String) -> Void)?
    /// What the picker selected: ink, or the eraser.
    var picked: PickedTool

    /// The id of the custom brush the pen draws with, if any.
    var activeCustomBrush: String? {
        if case .ink(let selection) = picked { return selection.brush.custom?.id }
        return nil
    }

    /// Draw with a library brush at the current colour and width (the
    /// iOS 17 sheet; iOS 18 goes through the picker's custom items).
    func pick(custom brush: LibraryBrush) {
        var color: UInt32 = 0x0000_00FF
        var width: Float = 8
        if case .ink(let current) = picked {
            color = current.color | 0xff
            width = current.brush.baseWidth
        }
        picked = .ink(
            BrushSelection(
                brush: BrushRef(
                    tool: brush.tool, baseWidth: width,
                    custom: CustomBrush(id: brush.id, spec: brush.spec)),
                color: color))
    }
    /// `-tool <pen|pencil|marker|monoline|fountain|crayon>` pins the tool
    /// and skips the picker (UI tests).
    let toolOverride: BrushSelection?

    /// The status line the UI tests read.
    var status: String {
        String(
            format: "strokes=%d shapes=%d wetSent=%d wetRecv=%d est=%d custom=%d originY=%.0f inline=%d boxY=%.0f",
            strokeCount, shapeCount, wetSent, wetRecv, estUpdated, customCount, originY,
            inlineCount, boxY)
    }

    private weak var renderer: InkRenderer?
    private weak var layout: (any LineLayoutProvider)?
    /// Committed overlay element ids on screen, in CRDT order.
    private var ids: [String] = []
    /// Every committed element on screen (both layers): where it lives
    /// and where that is on the page.
    private var placed: [String: (placement: Placement, origin: CGPoint)] = [:]
    /// Every committed element on screen, for re-showing and `createdMs`.
    private var elements: [String: Element] = [:]
    /// Inline element ids per loaded sketch, in CRDT order.
    private var inlineIds: [String: [String]] = [:]
    /// Loaded sketches in box (text) order.
    private var inlineOrder: [String] = []
    /// The subset of `ids` that are shapes.
    private var shapeIds: Set<String> = []
    /// The subset of `ids` that are strokes with a custom brush.
    private var customIds: Set<String> = []
    /// Remote wet strokes' placements, for re-placing.
    private var wetPlacements: [String: Placement] = [:]
    private var penDown = false
    /// Sketches that changed remotely while the pen was down.
    private var pendingSketches: Set<String> = []
    /// The remote pointer as last sent: when, where (page space) and
    /// whether the pen was down; `pointerSentPos == nil` once withdrawn.
    private var pointerSentAt: CFAbsoluteTime = 0
    private var pointerSentPos: CGPoint?
    private var pointerSentDown = false
    private var pendingRefresh = false
    private var erasing = false
    private var live: LiveStroke?
    /// Pen-up strokes waiting for estimated-property updates, by id.
    private var settling: [String: LiveStroke] = [:]
    private var flushTask: Task<Void, Never>?
    private var selfTestDone = false
    /// An estimate update asked for a redraw; done once per run-loop pass.
    private var liveRedrawPending = false

    init(session: NoteSession) {
        self.session = session
        let name = UserDefaults.standard.string(forKey: "tool") ?? ""
        let override = StrokeCodec.selection(named: name)
        toolOverride = override
        picked = .ink(override ?? StrokeCodec.selection(named: "pen")!)
    }

    /// The model outlives its canvas (cached per note); a reattach gets a
    /// fresh renderer and layout, so forget what the old one showed.
    func attach(renderer: InkRenderer, layout: any LineLayoutProvider) {
        self.renderer = renderer
        self.layout = layout
        ids = []
        placed = [:]
        elements = [:]
        inlineIds = [:]
        inlineOrder = []
        shapeIds = []
        customIds = []
        wetPlacements = [:]
        renderer.removeAll()
        refreshFromCrdt()
        if UserDefaults.standard.bool(forKey: "figureEight"), !selfTestDone {
            selfTestDone = true
            drawFigureEight()
        }
    }

    func detach() {
        renderer = nil
        layout = nil
    }

    /// `-figureEight 1`: commit one stroke that crosses itself, for the
    /// marker self-overlap UI test (XCUITest drags are straight lines).
    /// Lemniscate centred at (300, 300) of the page, crossing there, arm
    /// tip at (440, 300).
    private func drawFigureEight() {
        guard case .ink(let selection) = picked else { return }
        let n = 240
        let samples = (0...n).map { i -> RawSample in
            let t = Float(i) / Float(n) * 2 * .pi
            return RawSample(
                x: 300 + 140 * sin(t), y: 300 + 140 * sin(t) * cos(t), force: 0.5,
                tMs: Double(i) * 8, tilt: nil, estimationId: nil, expectsUpdate: false)
        }
        beginStroke(selection, at: samples[0])
        penMoved(coalesced: Array(samples.dropFirst()), predicted: [])
        penEnded(cancelled: false)
    }

    private func updateCounts() {
        shapeCount = shapeIds.count
        strokeCount = ids.count - shapeCount
        customCount = customIds.count
        originY = ids.first.flatMap { placed[$0]?.origin.y } ?? 0
        inlineCount = inlineIds.values.reduce(0) { $0 + $1.count }
        boxY = layout?.inlineBoxes().first?.rect.minY ?? 0
    }

    /// Re-place every element in z: inline sketches (box order, then CRDT
    /// order) below the overlay (CRDT order).
    private func rezAll() {
        guard let renderer else { return }
        var z = 0
        for sketch in inlineOrder {
            for id in inlineIds[sketch] ?? [] {
                if let element = elements[id], let origin = placed[id]?.origin {
                    renderer.show(element, z: z, origin: origin)
                }
                z += 1
            }
        }
        for id in ids {
            if let element = elements[id], let origin = placed[id]?.origin {
                renderer.show(element, z: z, origin: origin)
            }
            z += 1
        }
    }

    // MARK: pen input (samples in page space)

    func penBegan(_ sample: RawSample) {
        penDown = true
        renderer?.clearHover()
        sendPointer(sample, down: true)
        let selection: BrushSelection
        switch picked {
        case .eraser(let eraser):
            erasing = true
            erase(at: sample, radius: Self.eraserRadius(eraser))
            return
        case .ink(let ink):
            // A custom brush may have been edited in its popover since
            // the picker reported it.
            selection = ink.refreshed()
        }
        beginStroke(selection, at: sample)
        flushTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: wetFlushInterval)
                self?.flushWet()
            }
        }
    }

    /// A pen-down inside a sketch's box draws into that sketch; anywhere
    /// else onto the line under the pen.
    private func beginStroke(_ selection: BrushSelection, at sample: RawSample) {
        guard let layout else { return }
        let at = CGPoint(x: CGFloat(sample.x), y: CGFloat(sample.y))
        let line = layout.line(at: at)
        guard let anchor = try? session.anchorAt(charIndex: UInt64(line.scalar)) else { return }
        let placement: Placement
        let origin: CGPoint
        let id: String
        if let box = layout.inlineBox(at: at) {
            guard
                let began = try? session.beginStroke(
                    sketch: box.sketch, tool: selection.brush.tool, color: selection.color,
                    baseWidth: selection.brush.baseWidth, spec: selection.brush.custom?.spec)
            else { return }
            id = began
            placement = .sketch(box.sketch)
            origin = box.origin
        } else {
            guard
                let began = try? session.beginPageStroke(
                    anchor: anchor, tool: selection.brush.tool, color: selection.color,
                    baseWidth: selection.brush.baseWidth, spec: selection.brush.custom?.spec)
            else { return }
            id = began
            placement = .line(anchor: anchor)
            origin = line.origin
        }
        var stroke = LiveStroke(
            id: id, modeler: BrushModeler.forBrush(brush: selection.brush), selection: selection,
            placement: placement, anchor: anchor, lineStart: line.scalar, origin: origin)
        let local = sample.translated(by: origin)
        stroke.wetBuffer = stroke.modeler.push(samples: [local])
        if sample.expectsUpdate { stroke.estPushed += 1 }
        if StrokeRecorder.enabled { stroke.recording = [local] }
        live = stroke
        armHold(at: local)
        showLive(predicted: [])
    }

    /// `coalesced` are the real samples since the last event; `predicted`
    /// is Apple's guess at the next few, drawn as a tail and discarded on
    /// the next event.
    func penMoved(coalesced: [RawSample], predicted: [RawSample]) {
        if let last = coalesced.last { sendPointer(last, down: true) }
        if erasing, case .eraser(let eraser) = picked {
            let radius = Self.eraserRadius(eraser)
            for sample in coalesced { erase(at: sample, radius: radius) }
            return
        }
        guard live != nil else { return }
        let origin = live!.origin
        let local = coalesced.map { $0.translated(by: origin) }
        let emitted = live!.modeler.push(samples: local)
        live!.wetBuffer.append(contentsOf: emitted)
        if live!.wetBuffer.count > wetBufferCap {
            live!.wetBuffer.removeFirst(live!.wetBuffer.count - wetBufferCap)
        }
        live!.estPushed += coalesced.filter(\.expectsUpdate).count
        live!.recording?.append(contentsOf: local)
        if let last = local.last { armHold(at: last) }
        showLive(predicted: predicted.map { $0.translated(by: origin) })
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
                scheduleLiveRedraw()
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

    /// Updates arrive in bursts (hundreds per stroke); redraw once per
    /// run-loop pass, and not at all when a move event redraws first.
    private func scheduleLiveRedraw() {
        guard !liveRedrawPending else { return }
        liveRedrawPending = true
        DispatchQueue.main.async { [weak self] in
            guard let self, liveRedrawPending else { return }
            showLive(predicted: [])
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

    /// The view the haptic is attached to (iOS 17.5+ canvas feedback).
    @ObservationIgnored weak var hapticView: UIView?

    private func snapHaptic() {
        if #available(iOS 17.5, *), let view = hapticView {
            let generator = UICanvasFeedbackGenerator(view: view)
            generator.alignmentOccurred(at: CGPoint(x: view.bounds.midX, y: view.bounds.midY))
        } else {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
        }
    }

    /// Pen-up commits the modelled stroke under the wet id
    /// (`finishPageStroke` sends the wet End frame) — at once, or after
    /// the estimated updates its points still expect (≤ `settleTimeout`,
    /// ink kept on screen). A cancelled touch sends Cancel so receivers
    /// drop the provisional ink.
    func penEnded(cancelled: Bool) {
        penDown = false
        pointerLifted()
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
        if fakeEstimates { fakeSettle(id) }
    }

    /// Deliver the revisions a real Pencil would, 60 ms after pen-up.
    private func fakeSettle(_ id: String) {
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: .milliseconds(60))
            guard let self, let stroke = settling[id] else { return }
            let revised = stroke.modeler.pendingEstimates().map {
                RawSample(
                    x: 0, y: 0, force: 0.9, tMs: 0, tilt: nil, estimationId: $0,
                    expectsUpdate: false)
            }
            penEstimateUpdated(revised)
        }
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
            switch stroke.placement {
            case .line(let anchor):
                try? session.finishPageShape(shape: shape, anchor: anchor)
                shapeIds.insert(stroke.id)
            case .sketch(let sketch):
                try? session.finishShape(sketch: sketch, shape: shape)
            }
            element = .shape(shape)
        } else {
            let points = stroke.modeler.finish()
            let tail = Array(points.dropFirst(min(stroke.sentPoints, points.count)))
            let committed = Stroke(
                id: stroke.id, tool: stroke.brush.tool, color: stroke.color,
                baseWidth: stroke.brush.baseWidth, kind: .polylineSample, points: points,
                createdMs: createdMs, brush: stroke.brush.custom)
            switch stroke.placement {
            case .line(let anchor):
                try? session.finishPageStroke(stroke: committed, anchor: anchor, tail: tail)
            case .sketch(let sketch):
                try? session.finishStroke(sketch: sketch, stroke: committed, tail: tail)
            }
            element = .stroke(committed)
        }
        estUpdated += stroke.estUpdated
        if stroke.estPushed > 0 {
            let waited = stroke.endedAt.map { Date().timeIntervalSince($0) * 1000 } ?? 0
            NSLog(
                "est: pushed=%d updated=%d late=%d maxLateMs=%.0f waitedMs=%.0f unresolved=%d redraws=%d maxRedrawMs=%.1f",
                stroke.estPushed, stroke.estUpdated, stroke.estLate, stroke.maxLateMs, waited,
                stroke.modeler.pendingEstimates().count, stroke.redraws, stroke.maxRedrawMs)
        }
        elements[stroke.id] = element
        placed[stroke.id] = (stroke.placement, stroke.origin)
        switch stroke.placement {
        case .line:
            // `show` also drops the settling copy of the same id.
            let base = inlineIds.values.reduce(0) { $0 + $1.count }
            renderer?.show(element, z: base + ids.count, origin: stroke.origin)
            ids.append(stroke.id)
            if stroke.brush.custom != nil, stroke.shape == nil { customIds.insert(stroke.id) }
            updateCounts()
        case .sketch(let sketch):
            if inlineIds[sketch] == nil {
                inlineIds[sketch] = []
                inlineOrder.append(sketch)
            }
            inlineIds[sketch]!.append(stroke.id)
            // Below every overlay element: re-place the lot (`show` also
            // drops the settling copy of the same id).
            rezAll()
            updateCounts()
            onSketchChanged?(sketch)
        }
    }

    private func showLive(predicted: [RawSample]) {
        liveRedrawPending = false
        guard let stroke = live, let renderer else { return }
        let started = CFAbsoluteTimeGetCurrent()
        if let shape = stroke.shape {
            renderer.setLocalShape(shape, brush: stroke.brush, color: stroke.color, origin: stroke.origin)
        } else {
            // Meshed inside the modeler: no point crosses the FFI per event.
            renderer.setLocal(
                mesh: stroke.modeler.liveMesh(
                    predict: predicted, brush: stroke.brush, color: stroke.color,
                    tolerance: renderer.tolerance),
                origin: stroke.origin)
        }
        let ms = (CFAbsoluteTimeGetCurrent() - started) * 1000
        live!.redraws += 1
        live!.maxRedrawMs = max(live!.maxRedrawMs, ms)
    }

    // MARK: hover

    /// The Pencil hovering over the page: preview the tip at that spot
    /// and tilt at reduced alpha; `nil` when it leaves. Nothing while the
    /// pen is down or the eraser is selected.
    func hover(_ sample: RawSample?) {
        // Hover ends at touch-down, right before `penBegan`: withdraw the
        // remote pointer only when the pen really left.
        if let sample {
            sendPointer(sample, down: false)
        } else if !penDown {
            pointerGone()
        }
        guard let renderer else { return }
        guard let sample, !penDown, case .ink(let selection) = picked else {
            renderer.clearHover()
            return
        }
        let alpha = UInt32((Float(selection.color & 0xff) * hoverAlpha).rounded())
        renderer.setHover(
            mesh: hoverDabMesh(
                brush: selection.brush, color: (selection.color & ~0xff) | alpha,
                x: sample.x, y: sample.y, tilt: sample.tilt, tolerance: renderer.tolerance))
    }

    // MARK: remote pointer

    /// What the pointer draws: the picked tool, or `nil` for the eraser
    /// with its diameter as the width.
    private var pointerTool: (tool: Tool?, color: UInt32, width: Float) {
        switch picked {
        case .ink(let selection):
            return (selection.brush.tool, selection.color, selection.brush.baseWidth)
        case .eraser(let eraser):
            return (nil, 0, Self.eraserRadius(eraser) * 2)
        }
    }

    /// Tell peers where the pen is, in the space of the line under it. A
    /// change between hovering and drawing goes out at once; otherwise
    /// sends are throttled to `pointerInterval` and skipped until the pen
    /// moved `pointerMinMove` screen points.
    private func sendPointer(_ sample: RawSample, down: Bool) {
        let now = CFAbsoluteTimeGetCurrent()
        let at = CGPoint(x: CGFloat(sample.x), y: CGFloat(sample.y))
        if down == pointerSentDown, let last = pointerSentPos {
            if now - pointerSentAt < pointerInterval { return }
            let zoom = max(renderer?.viewport.zoom ?? 1, 0.01)
            if hypot(at.x - last.x, at.y - last.y) < pointerMinMove / zoom { return }
        }
        guard let layout else { return }
        // While drawing, the pointer rides the line under the stroke (the
        // embed line for an inline stroke).
        let scalar: Int
        let origin: CGPoint
        if let live {
            scalar = live.lineStart
            origin = layout.origin(forScalar: scalar)
        } else {
            (scalar, origin) = layout.line(at: at)
        }
        guard let anchor = try? session.anchorAt(charIndex: UInt64(scalar)) else { return }
        let tool = pointerTool
        let local = sample.translated(by: origin)
        try? session.sendPagePointer(
            anchor: anchor, x: local.x, y: local.y, tilt: sample.tilt, tool: tool.tool,
            color: tool.color, baseWidth: tool.width, down: down)
        pointerSentAt = now
        pointerSentPos = at
        pointerSentDown = down
    }

    /// Pen-up without a sample: repeat the last position as hovering so a
    /// non-hovering Pencil does not leave a "drawing" pointer behind.
    private func pointerLifted() {
        guard pointerSentDown, let last = pointerSentPos else { return }
        // Not a throttled move: clear the last send so this one goes out.
        pointerSentPos = nil
        pointerSentDown = false
        sendPointer(
            RawSample(
                x: Float(last.x), y: Float(last.y), force: 0, tMs: 0, tilt: nil,
                estimationId: nil, expectsUpdate: false),
            down: false)
    }

    /// The pen left the page (or the note closed): peers drop the pointer
    /// now rather than when it goes stale.
    func pointerGone() {
        guard pointerSentPos != nil else { return }
        pointerSentPos = nil
        pointerSentDown = false
        try? session.sendPagePointerGone()
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
        let sketches = pendingSketches
        pendingSketches = []
        for sketch in sketches { reloadSketch(sketch) }
    }

    // MARK: eraser

    /// Hit-test every element whose ink could be under the pen, each in
    /// its own space, and drop the ones the core says it touched: the
    /// overlay by probes, then every box the pen reaches into.
    private func erase(at sample: RawSample, radius: Float) {
        guard let renderer, let layout else { return }
        let at = CGPoint(x: CGFloat(sample.x), y: CGFloat(sample.y))
        let reach = CGFloat(radius)
        let probes: [PageProbe] = ids.compactMap { id in
            guard let bounds = renderer.bounds(for: id), let origin = placed[id]?.origin,
                  bounds.insetBy(dx: -reach, dy: -reach).contains(at)
            else { return nil }
            let local = sample.translated(by: origin)
            return PageProbe(element: id, x: local.x, y: local.y)
        }
        if !probes.isEmpty,
           let removed = try? session.erasePageAt(probes: probes, radius: radius),
           !removed.isEmpty
        {
            forget(Set(removed))
        }
        for box in layout.inlineBoxes()
        where box.rect.insetBy(dx: -reach, dy: -reach).contains(at) && inlineIds[box.sketch] != nil {
            let local = sample.translated(by: box.origin)
            guard
                let removed = try? session.eraseAt(
                    sketch: box.sketch, x: local.x, y: local.y, radius: radius),
                !removed.isEmpty
            else { continue }
            forget(Set(removed))
            onSketchChanged?(box.sketch)
        }
    }

    private func forget(_ gone: Set<String>) {
        for id in gone {
            renderer?.remove(id)
            placed[id] = nil
            elements[id] = nil
        }
        ids.removeAll { gone.contains($0) }
        for sketch in inlineOrder { inlineIds[sketch]?.removeAll { gone.contains($0) } }
        shapeIds.subtract(gone)
        customIds.subtract(gone)
        updateCounts()
    }

    private static func eraserRadius(_ eraser: PKEraserTool) -> Float {
        if #available(iOS 16.4, *) {
            return Float(max(4, eraser.width / 2))
        }
        return 12
    }

    // MARK: inbound wet ink

    func remoteWetBegin(
        stroke: String, anchor: Data, tool: Tool, color: UInt32, baseWidth: Float, spec: Data?
    ) {
        remoteWetBegin(
            stroke: stroke, placement: .line(anchor: anchor), tool: tool, color: color,
            baseWidth: baseWidth, spec: spec)
    }

    /// An inline wet stroke; dropped while its box is not on the page
    /// (no embed line yet): the commit brings it back.
    func remoteWetBegin(
        sketch: String, stroke: String, tool: Tool, color: UInt32, baseWidth: Float, spec: Data?
    ) {
        remoteWetBegin(
            stroke: stroke, placement: .sketch(sketch), tool: tool, color: color,
            baseWidth: baseWidth, spec: spec)
    }

    private func remoteWetBegin(
        stroke: String, placement: Placement, tool: Tool, color: UInt32, baseWidth: Float,
        spec: Data?
    ) {
        guard let origin = origin(of: placement) else { return }
        // The id does not matter for meshing; the committed stroke brings its own.
        let custom = spec.map { CustomBrush(id: "wet", spec: $0) }
        wetPlacements[stroke] = placement
        renderer?.wetBegin(
            stroke, brush: BrushRef(tool: tool, baseWidth: baseWidth, custom: custom), color: color,
            origin: origin)
    }

    func remoteWetPoints(stroke: String, points: [StrokePoint]) {
        wetRecv += 1
        renderer?.wetAppend(stroke, points)
    }

    /// Sender says no stroke is coming: drop the provisional ink right away.
    func remoteWetCancel(stroke: String) {
        wetPlacements[stroke] = nil
        renderer?.wetRemove(stroke)
    }

    func remoteWetEnd(stroke: String) {
        // The sender's tail arrived as points; finish the mesh with its end
        // taper. Keep the wet ink until the committed stroke lands
        // (pageChanged → refresh) so ink never blinks out; drop it only
        // as a fallback when no commit ever arrives.
        renderer?.wetEnd(stroke, tail: [])
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: wetLinger)
            self?.wetPlacements[stroke] = nil
            self?.renderer?.wetRemove(stroke)
        }
    }

    // MARK: anchors → page

    /// Where an anchor's line is on the page now; an anchor the note does
    /// not know goes to the last line.
    private func origin(of anchor: Data) -> CGPoint {
        guard let layout else { return .zero }
        let scalar = ((try? session.resolveAnchor(anchor: anchor)) ?? nil).map { Int($0) } ?? Int.max
        return layout.origin(forScalar: scalar)
    }

    /// Where a placement is on the page now; `nil` for a sketch whose box
    /// is not laid out.
    private func origin(of placement: Placement) -> CGPoint? {
        switch placement {
        case .line(let anchor): return origin(of: anchor)
        case .sketch(let sketch):
            return layout?.inlineBoxes().first(where: { $0.sketch == sketch })?.origin
        }
    }

    private func anchor(of placement: Placement) -> Data? {
        if case .line(let anchor) = placement { return anchor }
        return nil
    }

    /// The text was laid out again: re-place every element, wet stroke
    /// and the stroke in progress on its line or in its box.
    func layoutChanged() {
        guard let renderer, let layout else { return }
        var moved: [String: CGPoint] = [:]
        if !ids.isEmpty {
            let anchors = ids.map { placed[$0].flatMap { anchor(of: $0.placement) } ?? Data() }
            let resolved = (try? session.resolveAnchors(anchors: anchors)) ?? []
            for (i, id) in ids.enumerated() {
                let scalar = i < resolved.count ? resolved[i].map { Int($0) } ?? Int.max : Int.max
                let origin = layout.origin(forScalar: scalar)
                if placed[id]?.origin != origin {
                    placed[id]?.origin = origin
                    moved[id] = origin
                }
            }
            if !moved.isEmpty { renderer.setOrigins(moved) }
        }
        syncInline()
        for (id, placement) in wetPlacements {
            if let origin = origin(of: placement) { renderer.setWetOrigin(id, origin) }
        }
        for id in settling.keys {
            guard let origin = origin(of: settling[id]!.placement) else { continue }
            settling[id]!.origin = origin
            renderer.setSettlingOrigin(id, origin)
        }
        if live != nil, let origin = origin(of: live!.placement), origin != live!.origin {
            live!.origin = origin
            renderer.setLocalOrigin(origin)
        }
        updateCounts()
    }

    // MARK: inline sketches

    /// Match the loaded sketches to the boxes on the page: load the
    /// sketches whose box appeared, move the ones still there, drop the
    /// ones whose box is gone (their data stays in the note).
    private func syncInline() {
        guard let renderer, let layout else { return }
        let boxes = layout.inlineBoxes()
        let present = boxes.map(\.sketch)
        var changed = false
        for sketch in inlineOrder where !present.contains(sketch) {
            unloadSketch(sketch)
            changed = true
        }
        var moved: [String: CGPoint] = [:]
        for box in boxes {
            if inlineIds[box.sketch] == nil {
                loadSketch(box.sketch, origin: box.origin)
                changed = true
                continue
            }
            for id in inlineIds[box.sketch] ?? [] where placed[id]?.origin != box.origin {
                placed[id]?.origin = box.origin
                moved[id] = box.origin
            }
        }
        if !moved.isEmpty { renderer.setOrigins(moved) }
        if inlineOrder != present.filter({ inlineIds[$0] != nil }) {
            inlineOrder = present.filter { inlineIds[$0] != nil }
            changed = true
        }
        if changed { rezAll() }
    }

    /// Read a sketch's elements from the note and place them at `origin`
    /// (z is assigned by `rezAll`). An unknown container loads empty.
    private func loadSketch(_ sketch: String, origin: CGPoint) {
        let found = (try? session.elements(sketch: sketch)) ?? []
        inlineIds[sketch] = found.map(\.id)
        if !inlineOrder.contains(sketch) { inlineOrder.append(sketch) }
        for element in found {
            elements[element.id] = element
            placed[element.id] = (.sketch(sketch), origin)
            // A committed element replaces its wet ink.
            if renderer?.hasWet(element.id) == true {
                wetPlacements[element.id] = nil
                renderer?.wetRemove(element.id)
            }
        }
    }

    private func unloadSketch(_ sketch: String) {
        for id in inlineIds[sketch] ?? [] {
            renderer?.remove(id)
            placed[id] = nil
            elements[id] = nil
        }
        inlineIds[sketch] = nil
        inlineOrder.removeAll { $0 == sketch }
    }

    /// A sketch changed in the CRDT (remote commit or erase): reload it
    /// if its box is on the page. Deferred while a local pen is down,
    /// like `remoteChanged`.
    func remoteSketchChanged(sketch: String) {
        if penDown {
            pendingSketches.insert(sketch)
            return
        }
        reloadSketch(sketch)
    }

    private func reloadSketch(_ sketch: String) {
        guard let layout else { return }
        if inlineIds[sketch] != nil { unloadSketch(sketch) }
        if let box = layout.inlineBoxes().first(where: { $0.sketch == sketch }) {
            loadSketch(sketch, origin: box.origin)
        }
        inlineOrder = layout.inlineBoxes().map(\.sketch).filter { inlineIds[$0] != nil }
        rezAll()
        updateCounts()
        onSketchChanged?(sketch)
    }

    // MARK: CRDT → renderer

    func remoteChanged() {
        if penDown {
            pendingRefresh = true
        } else {
            refreshFromCrdt()
        }
    }

    /// Erase helper for the toolbar (and UI tests): drops the newest
    /// element of either layer (by creation time; the overlay wins a tie)
    /// through the same CRDT path the eraser uses.
    func eraseLast() {
        let overlay = ids.last
        let inline = inlineOrder.flatMap { inlineIds[$0] ?? [] }
            .max { (elements[$0]?.createdMs ?? 0) < (elements[$1]?.createdMs ?? 0) }
        let overlayMs = overlay.flatMap { elements[$0]?.createdMs } ?? 0
        let inlineMs = inline.flatMap { elements[$0]?.createdMs } ?? 0
        if let inline, overlay == nil || inlineMs > overlayMs,
           case .sketch(let sketch)? = placed[inline]?.placement
        {
            try? session.removeElement(sketch: sketch, element: inline)
            forget([inline])
            onSketchChanged?(sketch)
            return
        }
        guard let overlay else { return }
        try? session.removePageElement(element: overlay)
        forget([overlay])
    }

    private func refreshFromCrdt() {
        guard let renderer, let layout, let page = try? session.pageElements() else { return }
        let newIds = page.map(\.element.id)
        let keep = Set(newIds)
        for id in ids where !keep.contains(id) {
            renderer.remove(id)
            placed[id] = nil
            elements[id] = nil
        }
        let base = inlineIds.values.reduce(0) { $0 + $1.count }
        for (i, entry) in page.enumerated() {
            let scalar = entry.charIndex.map { Int($0) } ?? Int.max
            let origin = layout.origin(forScalar: scalar)
            placed[entry.element.id] = (.line(anchor: entry.anchor), origin)
            elements[entry.element.id] = entry.element
            renderer.show(entry.element, z: base + i, origin: origin)
        }
        ids = newIds
        shapeIds = Set(page.map(\.element).filter(\.isShape).map(\.id))
        customIds = Set(
            page.compactMap { entry in
                if case .stroke(let s) = entry.element, s.brush != nil { return s.id }
                return nil
            })
        // A committed element replaces its wet ink.
        for id in newIds where renderer.hasWet(id) {
            wetPlacements[id] = nil
            renderer.wetRemove(id)
        }
        updateCounts()
    }
}
