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
// Toolbar tools (`CanvasTool`) take over the pen from the picker: a preset
// shape is dragged out corner to corner (a line or arrow end to end, its
// ends binding to the rect, diamond or ellipse they land on); the select
// tool picks the topmost element, drags it (re-anchoring page ink to the
// line it is dropped on), resizes a shape from its outline and re-binds an
// arrow's ends. Arrows bound to a shape follow it live while it is dragged
// and are re-routed in the same commit, so peers see both move together.
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

/// A preset the shape tool draws.
enum ShapeKind: String, CaseIterable, Identifiable {
    case rect, diamond, ellipse, line, arrow
    var id: String { rawValue }

    var title: String {
        switch self {
        case .rect: "Rectangle"
        case .diamond: "Diamond"
        case .ellipse: "Ellipse"
        case .line: "Line"
        case .arrow: "Arrow"
        }
    }

    var symbol: String {
        switch self {
        case .rect: "square"
        case .diamond: "diamond"
        case .ellipse: "circle"
        case .line: "line.diagonal"
        case .arrow: "arrow.up.right"
        }
    }

    /// Lines and arrows have ends that bind; the rest enclose an area.
    var hasEnds: Bool { self == .line || self == .arrow }
}

/// What the pen does (toolbar). Shapes are styled by the picker's brush.
enum CanvasTool: Hashable {
    case draw
    case shape(ShapeKind)
    case select
}

/// A shape being dragged out with the shape tool; points in page space.
@MainActor
private struct ShapeDraft {
    /// The wet id the shape commits under.
    let id: String
    let kind: ShapeKind
    let selection: BrushSelection
    let placement: Placement
    /// Where the shape's space is on the page.
    let origin: CGPoint
    let start: CGPoint
    var end: CGPoint
    var startBinding: Binding?
    var endBinding: Binding?

    /// The shape in its own space: the box from `start` to `end`, or the
    /// segment between them.
    var shape: KrabinkCore.Shape {
        let a = Point2(x: Float(start.x - origin.x), y: Float(start.y - origin.y))
        let b = Point2(x: Float(end.x - origin.x), y: Float(end.y - origin.y))
        let center = Point2(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2)
        let size = Point2(x: max(abs(b.x - a.x), 1), y: max(abs(b.y - a.y), 1))
        switch kind {
        case .rect: return .rect(center: center, size: size, angle: 0)
        case .diamond: return .diamond(center: center, size: size, angle: 0)
        case .ellipse:
            return .ellipse(center: center, radii: Point2(x: size.x / 2, y: size.y / 2), angle: 0)
        case .line: return .line(a: a, b: b)
        case .arrow: return .arrow(a: a, b: b)
        }
    }

    var element: ShapeElement {
        ShapeElement(
            id: id, shape: shape, tool: selection.brush.tool, color: selection.color,
            width: selection.brush.baseWidth, start: startBinding, end: endBinding,
            createdMs: UInt64(max(0, Date().timeIntervalSince1970 * 1000)))
    }
}

/// A select-tool drag of one committed element; points in page space.
@MainActor
private struct Drag {
    enum Kind {
        /// Translate the element.
        case move
        /// Resize a shape from the outline point under the pen (its space).
        case resize(from: Point2)
        /// Turn the shape about its frame's centre (its space).
        case rotate(center: Point2)
        /// Scale the shape uniformly about the corner opposite the handle
        /// under the pen (its space).
        case scale(anchor: Point2)
        /// Move a line's or arrow's `a` (or `b`) end, re-binding it.
        case end(a: Bool)
    }

    let id: String
    let placement: Placement
    /// Where the element's space was on the page at pen-down.
    let origin: CGPoint
    /// The element as committed at pen-down.
    let element: Element
    let kind: Kind
    let from: CGPoint
    var to: CGPoint
    /// What the drag would commit (resize and end drags).
    var edited: ShapeElement?
    var delta: CGPoint { CGPoint(x: to.x - from.x, y: to.y - from.y) }
}

/// Screen points within which the pen grabs an arrow end or a shape's
/// outline for resizing, binds an arrow end to a shape, or selects ink.
private let handleReach: CGFloat = 14
private let bindReach: CGFloat = 12
private let selectReach: CGFloat = 8
/// The select tool's outline and the bind highlight (systemBlue).
private let selectionColor: UInt32 = 0x3478_F6CC
/// The selected element's ink while it is selected: the same blue,
/// opaque so it hides the ink's own colour. Never stored.
private let selectionTint: UInt32 = 0x3478_F6FF
/// Screen points the selection box clears the shape's frame by, and how
/// far above its top edge the rotate knob sits. The pad keeps a line's
/// corner handles clear of its end dots (`handleReach` apart at least).
private let selectionPad: CGFloat = 12
private let knobOffset: CGFloat = 24
/// Rotation snaps to multiples of 15° when within 3° of one.
private let rotateSnapStep: Float = .pi / 12
private let rotateSnapWithin: Float = 3 * .pi / 180

/// Where the select tool's handles sit for one shape, in the shape's
/// space: a box padded out from its frame, corner handles on that box, a
/// rotate knob above the top edge. Shared by hit testing and drawing.
private struct SelectionFrame {
    /// The shape's own box, unpadded.
    let frame: KrabinkCore.Frame
    /// Half the padded box, along its axes.
    let half: Point2
    let knobOffset: Float

    init(of shape: ShapeElement, zoom: CGFloat) {
        frame = shapeFrame(shape: shape.shape)
        let pad = Float(selectionPad / zoom) + shape.width / 2
        half = Point2(x: frame.size.x / 2 + pad, y: frame.size.y / 2 + pad)
        knobOffset = Float(Krabink.knobOffset / zoom)
    }

    var center: Point2 { frame.center }
    var paddedSize: Point2 { Point2(x: half.x * 2, y: half.y * 2) }

    /// The point `(dx, dy)` along the box's axes from its centre.
    private func place(_ dx: Float, _ dy: Float) -> Point2 {
        Point2(x: dx, y: dy).rotated(by: frame.angle, about: .zero).offset(by: center)
    }

    /// Top-left, top-right, bottom-right, bottom-left of the padded box.
    var corners: [Point2] {
        [place(-half.x, -half.y), place(half.x, -half.y), place(half.x, half.y), place(-half.x, half.y)]
    }

    /// The shape's own corner diagonally opposite padded corner `i`: what
    /// stays put when that corner is dragged.
    func anchor(opposite i: Int) -> Point2 {
        let (hx, hy) = (frame.size.x / 2, frame.size.y / 2)
        let signs: [(Float, Float)] = [(1, 1), (-1, 1), (-1, -1), (1, -1)]
        return place(signs[i].0 * hx, signs[i].1 * hy)
    }

    /// Where the rotate knob's stem leaves the box, and the knob.
    var top: Point2 { place(0, -half.y) }
    var knob: Point2 { place(0, -(half.y + knobOffset)) }

    /// `p` in the box's own axes, centre at the origin.
    func inFrame(_ p: Point2) -> Point2 {
        Point2(x: p.x - center.x, y: p.y - center.y).rotated(by: -frame.angle, about: .zero)
    }
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
    /// What the picker selected: ink, or the eraser. Picking the eraser
    /// leaves the toolbar tools.
    var picked: PickedTool {
        didSet {
            if case .eraser = picked { tool = .draw }
        }
    }
    /// The toolbar tool: draw with the picker's brush, drag out a preset
    /// shape, or select and move.
    var tool: CanvasTool = .draw {
        didSet {
            if tool != oldValue { select(nil) }
        }
    }
    /// The select tool's element, if any.
    private(set) var selected: String?

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
            format: "strokes=%d shapes=%d wetSent=%d wetRecv=%d est=%d custom=%d originY=%.0f inline=%d boxY=%.0f sel=%d bound=%d angle=%.0f size=%.0fx%.0f",
            strokeCount, shapeCount, wetSent, wetRecv, estUpdated, customCount, originY,
            inlineCount, boxY, selected == nil ? 0 : 1, boundCount, selectedAngle,
            selectedFrame?.size.x ?? 0, selectedFrame?.size.y ?? 0)
    }

    /// The selected shape's frame as committed; nil for strokes and no
    /// selection.
    private var selectedFrame: KrabinkCore.Frame? {
        guard let id = selected, case .shape(let s)? = elements[id] else { return nil }
        return shapeFrame(shape: s.shape)
    }

    /// The selected shape's angle in whole degrees, (-180, 180].
    private var selectedAngle: Double {
        let deg = (Double(selectedFrame?.angle ?? 0) * 180 / .pi).rounded()
        return deg == 0 ? 0 : deg
    }

    /// Lines and arrows on screen with at least one bound end.
    private var boundCount: Int {
        elements.values.filter { element in
            if case .shape(let s) = element { return s.start != nil || s.end != nil }
            return false
        }.count
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
    /// The next pen-down is a claimed finger's (`claimsFinger`).
    private var fingerNext = false
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
    private var draft: ShapeDraft?
    private var drag: Drag?
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
        selected = nil
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
        showSelection()
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
        let finger = fingerNext
        fingerNext = false
        if finger {
            beginSelect(at: sample)
            return
        }
        // The pen is back at its own tool: a finger's selection goes.
        if tool != .select { select(nil) }
        switch tool {
        case .shape(let kind):
            beginShape(kind, at: sample)
            return
        case .select:
            beginSelect(at: sample)
            return
        case .draw: break
        }
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
        if draft != nil || drag != nil {
            if let last = coalesced.last { toolMoved(to: last.point) }
            return
        }
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
        if draft != nil {
            endShape(cancelled: cancelled)
            return
        }
        if drag != nil {
            endDrag(cancelled: cancelled)
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
            // Same id as the wet stream: receivers swap ink for shape. A
            // recognised line or arrow binds its ends like a drawn one.
            var shape = ShapeElement(
                id: stroke.id, shape: snapped, tool: stroke.brush.tool, color: stroke.color,
                width: stroke.brush.baseWidth, start: nil, end: nil, createdMs: createdMs)
            if let ends = snapped.ends {
                shape.start = binding(
                    at: ends.a.page(from: stroke.origin), in: stroke.placement, exclude: nil)
                shape.end = binding(
                    at: ends.b.page(from: stroke.origin), in: stroke.placement, exclude: nil)
                shape = route(shape, at: stroke.origin)
            }
            switch stroke.placement {
            case .line(let anchor):
                try? session.finishPageShape(shape: shape, anchor: anchor)
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
        adopt(element, placement: stroke.placement, origin: stroke.origin)
    }

    /// A local commit landed: show it on top of its layer and count it.
    private func adopt(_ element: Element, placement: Placement, origin: CGPoint) {
        let id = element.id
        elements[id] = element
        placed[id] = (placement, origin)
        switch placement {
        case .line:
            // `show` also drops the settling copy of the same id.
            let base = inlineIds.values.reduce(0) { $0 + $1.count }
            renderer?.show(element, z: base + ids.count, origin: origin)
            ids.append(id)
            switch element {
            case .shape: shapeIds.insert(id)
            case .stroke(let s): if s.brush != nil { customIds.insert(id) }
            }
            updateCounts()
        case .sketch(let sketch):
            if inlineIds[sketch] == nil {
                inlineIds[sketch] = []
                inlineOrder.append(sketch)
            }
            inlineIds[sketch]!.append(id)
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
        guard let sample, !penDown, tool == .draw, case .ink(let selection) = picked else {
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
        if let selected, gone.contains(selected) { self.selected = nil }
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
            if !moved.isEmpty {
                renderer.setOrigins(moved)
                // Lines moved apart or together: bound arrows follow.
                reroutePage()
            }
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
        if let selected, placed[selected] == nil { self.selected = nil }
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

    // MARK: shape tool

    private var zoom: CGFloat { max(renderer?.viewport.zoom ?? 1, 0.01) }

    /// The brush shapes are inked with: the picked one, a pen under the
    /// eraser. Shapes store a preset tool, so a custom brush's spec is not
    /// carried.
    private var shapeSelection: BrushSelection {
        let selection: BrushSelection
        if case .ink(let ink) = picked {
            selection = ink
        } else {
            selection = toolOverride ?? StrokeCodec.selection(named: "pen")!
        }
        var out = selection
        out.brush.custom = nil
        return out
    }

    /// Pen-down with the shape tool: the shape lives in the box or on the
    /// line under the pen, like a stroke, and commits under a wet id so
    /// peers can drop a cancelled one.
    private func beginShape(_ kind: ShapeKind, at sample: RawSample) {
        guard let layout else { return }
        let at = sample.point
        let selection = shapeSelection
        let brush = selection.brush
        let placement: Placement
        let origin: CGPoint
        let id: String?
        if let box = layout.inlineBox(at: at) {
            placement = .sketch(box.sketch)
            origin = box.origin
            id = try? session.beginStroke(
                sketch: box.sketch, tool: brush.tool, color: selection.color,
                baseWidth: brush.baseWidth, spec: nil)
        } else {
            let line = layout.line(at: at)
            guard let anchor = try? session.anchorAt(charIndex: UInt64(line.scalar)) else { return }
            placement = .line(anchor: anchor)
            origin = line.origin
            id = try? session.beginPageStroke(
                anchor: anchor, tool: brush.tool, color: selection.color,
                baseWidth: brush.baseWidth, spec: nil)
        }
        guard let id else { return }
        draft = ShapeDraft(
            id: id, kind: kind, selection: selection, placement: placement, origin: origin,
            start: at, end: at,
            startBinding: kind.hasEnds ? binding(at: at, in: placement, exclude: nil) : nil)
        showDraft()
    }

    private func toolMoved(to point: CGPoint) {
        if draft != nil {
            draft!.end = point
            if draft!.kind.hasEnds {
                draft!.endBinding = binding(at: point, in: draft!.placement, exclude: nil)
            }
            showDraft()
        } else if drag != nil {
            dragMoved(to: point)
        }
    }

    /// The draft as it would commit: bound ends routed to their targets.
    private var draftElement: ShapeElement? {
        draft.map { route($0.element, at: $0.origin) }
    }

    private func showDraft() {
        guard let draft, let shape = draftElement, let renderer else { return }
        renderer.setLocalShape(
            shape.shape, brush: BrushRef(tool: shape.tool, baseWidth: shape.width, custom: nil),
            color: shape.color, origin: draft.origin)
        showSelection(targets: [draft.startBinding, draft.endBinding])
    }

    /// Pen-up with the shape tool: commit the shape, or drop it when it is
    /// a tap rather than a drag.
    private func endShape(cancelled: Bool) {
        guard let draft else { return }
        let shape = draftElement
        self.draft = nil
        renderer?.clearLocal()
        showSelection()
        let span = hypot(draft.end.x - draft.start.x, draft.end.y - draft.start.y)
        guard !cancelled, span * zoom >= 4, let shape else {
            try? session.cancelStroke(stroke: draft.id)
            return
        }
        switch draft.placement {
        case .line(let anchor):
            try? session.finishPageShape(shape: shape, anchor: anchor)
        case .sketch(let sketch):
            try? session.finishShape(sketch: sketch, shape: shape)
        }
        adopt(.shape(shape), placement: draft.placement, origin: draft.origin)
    }

    // MARK: bindings

    /// The binding a line or arrow end at `point` (page space) in
    /// `placement`'s layer would take: the topmost closed shape there.
    private func binding(at point: CGPoint, in placement: Placement, exclude: String?) -> Binding? {
        let reach = Float(bindReach / zoom)
        switch placement {
        case .line:
            return (try? session.bindingAtPage(
                x: Float(point.x), y: Float(point.y), origins: pageOrigins(), reach: reach,
                exclude: exclude)) ?? nil
        case .sketch(let sketch):
            guard let origin = origin(of: placement) else { return nil }
            let local = point.local(to: origin)
            return (try? session.bindingAt(
                sketch: sketch, x: local.x, y: local.y, reach: reach, exclude: exclude)) ?? nil
        }
    }

    /// Every overlay element's line origin; `overrides` for elements
    /// mid-drag.
    private func pageOrigins(_ overrides: [String: CGPoint] = [:]) -> [ElementOrigin] {
        ids.compactMap { id in
            guard let o = overrides[id] ?? placed[id]?.origin else { return nil }
            return ElementOrigin(element: id, x: Float(o.x), y: Float(o.y))
        }
    }

    /// `arrow` (in the space at `origin`) with its bound ends re-derived
    /// from its targets as drawn now; `overrides` stand in for elements
    /// mid-drag. Targets may sit on other lines than the arrow.
    private func route(
        _ arrow: ShapeElement, at origin: CGPoint,
        overrides: [String: (element: Element, origin: CGPoint)] = [:]
    ) -> ShapeElement {
        let targets = Set([arrow.start?.element, arrow.end?.element].compactMap { $0 })
        guard !targets.isEmpty else { return arrow }
        var entries: [PageElement] = []
        var origins: [ElementOrigin] = []
        func add(_ element: Element, _ o: CGPoint) {
            entries.append(PageElement(element: element, anchor: Data(), charIndex: nil))
            origins.append(ElementOrigin(element: element.id, x: Float(o.x), y: Float(o.y)))
        }
        for id in targets {
            if let over = overrides[id] {
                add(over.element, over.origin)
            } else if let element = elements[id], let o = placed[id]?.origin {
                add(element, o)
            }
        }
        add(.shape(arrow), origin)
        guard case .shape(let routed)? = resolvePageBindings(elements: entries, origins: origins).last?.element
        else { return arrow }
        return routed
    }

    /// Re-route the bound arrows of a layer (`layer` ids) from where their
    /// targets are drawn. With `overrides` (a drag preview) only arrows
    /// bound to those elements, on screen only; otherwise every bound
    /// arrow, kept.
    private func reroute(
        _ layer: [String], overrides: [String: (element: Element, origin: CGPoint)] = [:]
    ) {
        for id in layer where overrides[id] == nil {
            guard case .shape(let arrow)? = elements[id], let origin = placed[id]?.origin else {
                continue
            }
            let targets = [arrow.start?.element, arrow.end?.element].compactMap { $0 }
            if targets.isEmpty { continue }
            if !overrides.isEmpty, !targets.contains(where: { overrides[$0] != nil }) { continue }
            let routed = route(arrow, at: origin, overrides: overrides)
            renderer?.update(.shape(routed))
            if overrides.isEmpty { elements[id] = .shape(routed) }
        }
    }

    private func reroutePage() { reroute(ids) }

    private func layerIds(_ placement: Placement) -> [String] {
        switch placement {
        case .line: ids
        case .sketch(let sketch): inlineIds[sketch] ?? []
        }
    }

    // MARK: select tool

    /// Pen-down with the select tool: grab the selected shape's end or
    /// outline, else pick the topmost element under the pen and drag it.
    private func beginSelect(at sample: RawSample) {
        let at = sample.point
        if let id = selected, let kind = handle(of: id, at: at) {
            startDrag(id, kind: kind, at: at)
            return
        }
        guard let hit = hitTest(at) else {
            select(nil)
            return
        }
        select(hit)
        startDrag(hit, kind: .move, at: at)
    }

    /// Whether a finger landing at `point` selects rather than going to
    /// the editor: it is on the selection's handle or on an element, so a
    /// tap selects and a drag moves. Elsewhere the finger scrolls or
    /// places the caret. With `anyTool` (fingers do not ink) that holds
    /// under every tool, and the toolbar's tool stays the pen's.
    func claimsFinger(at point: CGPoint, anyTool: Bool) -> Bool {
        guard !penDown, tool == .select || anyTool else { return false }
        if let id = selected, handle(of: id, at: point) != nil { return true }
        return hitTest(point) != nil
    }

    /// The pen-down that follows is a claimed finger's: it selects,
    /// whatever the tool.
    func fingerClaimed() {
        fingerNext = true
    }

    private func startDrag(_ id: String, kind: Drag.Kind, at point: CGPoint) {
        guard let element = elements[id], let place = placed[id] else { return }
        drag = Drag(
            id: id, placement: place.placement, origin: place.origin, element: element, kind: kind,
            from: point, to: point)
    }

    /// The selected element's handle under the pen, nearest first: a
    /// line's or arrow's end, the rotate knob, a corner of the selection
    /// box (scale), or near a closed shape's outline (resize). A line
    /// bound at both ends only offers its ends: turning or scaling it
    /// would re-route straight back.
    private func handle(of id: String, at point: CGPoint) -> Drag.Kind? {
        guard case .shape(let s)? = elements[id], let origin = placed[id]?.origin else { return nil }
        let reach = handleReach / zoom
        let local = point.local(to: origin)
        if let ends = s.shape.ends {
            if ends.a.distance(to: local) <= reach { return .end(a: true) }
            if ends.b.distance(to: local) <= reach { return .end(a: false) }
            if s.start != nil && s.end != nil { return nil }
        }
        let box = SelectionFrame(of: s, zoom: zoom)
        if box.knob.distance(to: local) <= reach { return .rotate(center: box.center) }
        if let i = box.corners.firstIndex(where: { $0.distance(to: local) <= reach }) {
            return .scale(anchor: box.anchor(opposite: i))
        }
        guard s.shape.ends == nil else { return nil }
        let q = box.inFrame(local)
        let outside = max(abs(q.x) - box.frame.size.x / 2, abs(q.y) - box.frame.size.y / 2)
        return abs(outside) <= Float(reach) ? .resize(from: local) : nil
    }

    /// The topmost element under the pen: overlay ink first (it is drawn
    /// above), then the inline sketch whose box the pen is in. A closed
    /// shape is hit anywhere inside.
    private func hitTest(_ point: CGPoint) -> String? {
        guard let renderer, let layout else { return nil }
        let reach = selectReach / zoom
        let probes: [PageProbe] = ids.compactMap { id in
            guard let bounds = renderer.bounds(for: id), let origin = placed[id]?.origin,
                  bounds.insetBy(dx: -reach, dy: -reach).contains(point)
            else { return nil }
            let local = point.local(to: origin)
            return PageProbe(element: id, x: local.x, y: local.y)
        }
        if !probes.isEmpty,
           let hit = (try? session.hitPageAt(probes: probes, radius: Float(reach))) ?? nil
        {
            return hit
        }
        guard let box = layout.inlineBox(at: point), inlineIds[box.sketch] != nil else { return nil }
        let local = point.local(to: box.origin)
        return (try? session.hitAt(
            sketch: box.sketch, x: local.x, y: local.y, radius: Float(reach))) ?? nil
    }

    /// Preview the drag: the element (and the arrows bound to it) where
    /// it would land. Nothing is committed until pen-up.
    private func dragMoved(to point: CGPoint) {
        guard var drag, let renderer else { return }
        drag.to = point
        var targets: [Binding?] = []
        switch drag.kind {
        case .move:
            let moved = CGPoint(x: drag.origin.x + drag.delta.x, y: drag.origin.y + drag.delta.y)
            renderer.setOrigins([drag.id: moved])
            reroute(layerIds(drag.placement), overrides: [drag.id: (drag.element, moved)])
        case .resize, .rotate, .scale:
            guard case .shape(var s) = drag.element else { break }
            let (from, to) = (drag.from.local(to: drag.origin), point.local(to: drag.origin))
            switch drag.kind {
            case .resize(let handle):
                s.shape = resizeShape(shape: s.shape, from: handle, to: to)
            case .rotate(let c):
                let by = atan2(to.y - c.y, to.x - c.x) - atan2(from.y - c.y, from.x - c.x)
                s.shape = rotateShape(
                    shape: s.shape, about: c, by: by, snapStep: rotateSnapStep,
                    snapWithin: rotateSnapWithin)
            case .scale(let anchor):
                // How far along the handle's diagonal the pen is now.
                let (f, t) = (from.offset(by: anchor.negated), to.offset(by: anchor.negated))
                let k = (f.x * t.x + f.y * t.y) / max(f.x * f.x + f.y * f.y, .ulpOfOne)
                s.shape = scaleShape(shape: s.shape, about: anchor, k: k)
            default: break
            }
            // A bound end goes back where its target says; show that.
            if s.start != nil || s.end != nil { s = route(s, at: drag.origin) }
            drag.edited = s
            renderer.update(.shape(s))
            reroute(layerIds(drag.placement), overrides: [drag.id: (.shape(s), drag.origin)])
        case .end(let atA):
            guard case .shape(var s) = drag.element, let ends = s.shape.ends else { break }
            let p = point.local(to: drag.origin)
            let bound = binding(at: point, in: drag.placement, exclude: drag.id)
            s.shape = s.shape.withEnds(atA ? p : ends.a, atA ? ends.b : p)
            if atA { s.start = bound } else { s.end = bound }
            s = route(s, at: drag.origin)
            drag.edited = s
            renderer.update(.shape(s))
            targets = [bound]
        }
        self.drag = drag
        showSelection(targets: targets)
    }

    /// Pen-up with the select tool: commit the drag (a tap only selects);
    /// a cancelled drag puts everything back.
    private func endDrag(cancelled: Bool) {
        guard let drag else { return }
        self.drag = nil
        let moved = hypot(drag.delta.x, drag.delta.y) * zoom >= 2
        if cancelled || !moved {
            if moved { reload(drag.placement) }
            showSelection()
            return
        }
        switch (drag.kind, drag.placement) {
        case (.move, .line):
            commitPageMove(drag)
        case (.move, .sketch(let sketch)):
            try? session.moveElements(
                sketch: sketch, elements: [drag.id], dx: Float(drag.delta.x),
                dy: Float(drag.delta.y))
        case (_, .line):
            if let edited = drag.edited {
                try? session.updatePageShape(shape: edited, anchor: nil, origins: pageOrigins())
            }
        case (_, .sketch(let sketch)):
            if let edited = drag.edited { try? session.updateShape(sketch: sketch, shape: edited) }
        }
        reload(drag.placement)
    }

    /// Commit an overlay move: re-anchor the element to the line its ink's
    /// top-left was dropped on, with the delta in that line's space, so it
    /// follows that line when the text reflows.
    private func commitPageMove(_ drag: Drag) {
        guard let layout, let renderer else { return }
        let dropped = CGPoint(x: drag.origin.x + drag.delta.x, y: drag.origin.y + drag.delta.y)
        let corner = renderer.bounds(for: drag.id)?.origin ?? dropped
        let line = layout.line(at: corner)
        guard let anchor = try? session.anchorAt(charIndex: UInt64(line.scalar)) else { return }
        let move = PageMove(
            element: drag.id, dx: Float(dropped.x - line.origin.x),
            dy: Float(dropped.y - line.origin.y), anchor: anchor)
        try? session.movePageElements(moves: [move], origins: pageOrigins([drag.id: line.origin]))
    }

    /// Re-read a layer from the CRDT after an edit (or to undo a preview).
    private func reload(_ placement: Placement) {
        switch placement {
        case .line: refreshFromCrdt()
        case .sketch(let sketch): reloadSketch(sketch)
        }
    }

    private func select(_ id: String?) {
        selected = id
        showSelection()
    }

    /// Toolbar: delete the selected element. Arrows bound to it stay where
    /// they are, unbound.
    func deleteSelected() {
        guard let id = selected, let placement = placed[id]?.placement else { return }
        switch placement {
        case .line:
            try? session.removePageElement(element: id)
        case .sketch(let sketch):
            try? session.removeElement(sketch: sketch, element: id)
        }
        forget([id])
        if case .sketch(let sketch) = placement { onSketchChanged?(sketch) }
    }

    /// Tint the selection and outline it with its handles (corners and
    /// rotate knob on a shape's box, dots on a line's or arrow's ends; a
    /// stroke gets a plain box) and the shapes a drawn or dragged arrow
    /// end binds to.
    /// Drawn from what the renderer shows, so handles follow a drag.
    private func showSelection(targets: [Binding?] = []) {
        guard let renderer else { return }
        let thin = BrushRef(tool: .monoline, baseWidth: Float(1.5 / zoom), custom: nil)
        let thick = BrushRef(tool: .monoline, baseWidth: Float(3 / zoom), custom: nil)
        let tolerance = renderer.tolerance
        func outline(_ shape: KrabinkCore.Shape, _ brush: BrushRef) -> InkMesh {
            shapeOutlineMesh(shape: shape, brush: brush, color: selectionColor, tolerance: tolerance)
        }
        let dotRadius = Float(5 / zoom)
        func dot(_ at: Point2) -> KrabinkCore.Shape {
            .ellipse(center: at, radii: Point2(x: dotRadius, y: dotRadius), angle: 0)
        }
        var meshes: [(InkMesh, CGPoint)] = []
        if let id = selected, let shown = renderer.shown(id) {
            // The selection's own ink again in the selection colour, over
            // the ink: the box alone is ambiguous where elements overlap.
            var tinted = shown.element
            switch tinted {
            case .shape(var s):
                s.color = selectionTint
                tinted = .shape(s)
            case .stroke(var s):
                s.color = selectionTint
                tinted = .stroke(s)
            }
            meshes.append((elementMesh(element: tinted, tolerance: tolerance), shown.origin))
            switch shown.element {
            case .shape(let s):
                let box = SelectionFrame(of: s, zoom: zoom)
                let rect = KrabinkCore.Shape.rect(
                    center: box.center, size: box.paddedSize, angle: box.frame.angle)
                meshes.append((outline(rect, thin), shown.origin))
                let fixed = s.shape.ends != nil && s.start != nil && s.end != nil
                if !fixed {
                    let side = Float(8 / zoom)
                    for corner in box.corners {
                        let handle = KrabinkCore.Shape.rect(
                            center: corner, size: Point2(x: side, y: side), angle: box.frame.angle)
                        meshes.append((outline(handle, thin), shown.origin))
                    }
                    meshes.append((outline(.line(a: box.top, b: box.knob), thin), shown.origin))
                    meshes.append((outline(dot(box.knob), thin), shown.origin))
                }
                if let ends = s.shape.ends {
                    for end in [ends.a, ends.b] {
                        meshes.append((outline(dot(end), thin), shown.origin))
                    }
                }
            case .stroke:
                if let bounds = renderer.bounds(for: id) {
                    let r = bounds.insetBy(dx: -selectionPad / zoom, dy: -selectionPad / zoom)
                    let rect = KrabinkCore.Shape.rect(
                        center: Point2(x: Float(r.midX), y: Float(r.midY)),
                        size: Point2(x: Float(r.width), y: Float(r.height)), angle: 0)
                    meshes.append((outline(rect, thin), .zero))
                }
            }
        }
        for target in targets.compactMap({ $0 }) {
            guard let shown = renderer.shown(target.element), case .shape(let s) = shown.element
            else { continue }
            meshes.append((outline(s.shape, thick), shown.origin))
        }
        if meshes.isEmpty {
            renderer.clearSelection()
        } else {
            renderer.setSelection(meshes: meshes)
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
        if let selected, placed[selected] == nil { self.selected = nil }
        let base = inlineIds.values.reduce(0) { $0 + $1.count }
        for (i, entry) in page.enumerated() {
            let scalar = entry.charIndex.map { Int($0) } ?? Int.max
            let origin = layout.origin(forScalar: scalar)
            placed[entry.element.id] = (.line(anchor: entry.anchor), origin)
            elements[entry.element.id] = entry.element
            renderer.show(entry.element, z: base + i, origin: origin)
        }
        ids = newIds
        // Bound arrows derive their ends from where their targets' lines
        // are drawn now.
        reroutePage()
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
