// iM2 PencilKit risk spike (simulator half). Launch with `-spike 1`.
//
// Questions this screen answers, surfaced in the status label (UI-test
// readable) and in `IM2` NSLog lines:
//  (a) Does canvasViewDrawingDidChange fire at stroke commit (pen-up),
//      never mid-stroke? -> didChange count vs strokes, midStroke flag.
//  (b) Does a passive gesture recognizer (stays .possible, cancels
//      nothing) see coalesced touches while PencilKit keeps drawing
//      normally? -> began/moved/ended/coalesced counters + strokes count.
//  (c) Does a committed PKStroke survive a rebuild through our CRDT
//      schema fields (x, y, force, t_ms, tilt as f32 + one base width)?
//      -> max pixel diff of full-fidelity vs schema-quantized rebuilds.
//
// Pressure/latency conclusions still need a real iPad; simulator input
// has constant force and no true coalescing.

import PencilKit
import PendantCore
import SwiftUI
import UIKit

@Observable @MainActor
final class SpikeModel {
    var strokes = 0
    var didChangeCount = 0
    var midStrokeDidChange = false
    var began = 0
    var moved = 0
    var ended = 0
    var coalesced = 0
    /// Same counters for the "active" observer variant (recognizes with
    /// simultaneous-recognition allowed instead of staying .possible).
    var activeBegan = 0
    var activeMoved = 0
    var activeEnded = 0
    /// Max pixel-channel diff, original vs rebuild with every PKStrokePoint
    /// field copied verbatim. Expect 0 (proves PKStrokePath(controlPoints:)
    /// reproduces rendering).
    var maxFull = -1
    /// Same, but points squeezed through our CRDT schema (f32 + ms + one
    /// stroke-level width). Nonzero here = schema needs more fields.
    var maxSchema = -1

    var status: String {
        "strokes=\(strokes) didChange=\(didChangeCount)"
            + " midStroke=\(midStrokeDidChange ? "yes" : "no")"
            + " began=\(began) moved=\(moved) ended=\(ended) coalesced=\(coalesced)"
            + " act=\(activeBegan)/\(activeMoved)/\(activeEnded)"
            + " maxFull=\(maxFull) maxSchema=\(maxSchema)"
    }
}

struct SpikeScreen: View {
    @State private var model = SpikeModel()

    var body: some View {
        VStack(spacing: 0) {
            Text(model.status)
                .font(.system(size: 13, design: .monospaced))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
                .accessibilityIdentifier("spikeStatus")
            SpikeCanvas(model: model)
        }
    }
}

/// Watches every touch on the canvas without ever recognizing: stays in
/// .possible, cancels nothing, delays nothing. This is the mechanism the
/// live wet-ink stream (iM4) would ride on.
final class TouchObserverGestureRecognizer: UIGestureRecognizer {
    var onTouches: ((UITouch.Phase, Set<UITouch>, UIEvent) -> Void)?

    override init(target: Any?, action: Selector?) {
        super.init(target: target, action: action)
        cancelsTouchesInView = false
        delaysTouchesBegan = false
        delaysTouchesEnded = false
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.began, touches, event)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.moved, touches, event)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.ended, touches, event)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.cancelled, touches, event)
    }
}

/// Variant B: actually recognizes (continuous state machine) but declares
/// simultaneous recognition with everything and cancels nothing. Finding
/// sought: does this variant receive touchesEnded (the passive one does
/// not once PencilKit's recognizer claims the gesture), and does PencilKit
/// still commit strokes normally alongside it?
final class ActiveObserverGestureRecognizer: UIGestureRecognizer, UIGestureRecognizerDelegate {
    var onTouches: ((UITouch.Phase, Set<UITouch>, UIEvent) -> Void)?

    override init(target: Any?, action: Selector?) {
        super.init(target: target, action: action)
        cancelsTouchesInView = false
        delaysTouchesBegan = false
        delaysTouchesEnded = false
        delegate = self
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.began, touches, event)
        state = .began
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.moved, touches, event)
        state = .changed
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.ended, touches, event)
        state = .ended
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        onTouches?(.cancelled, touches, event)
        state = .cancelled
    }

    func gestureRecognizer(
        _ gestureRecognizer: UIGestureRecognizer,
        shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer
    ) -> Bool {
        true
    }
}

struct SpikeCanvas: UIViewRepresentable {
    let model: SpikeModel

    func makeCoordinator() -> Coordinator { Coordinator(model: model) }

    func makeUIView(context: Context) -> PKCanvasView {
        let canvas = PKCanvasView()
        // .anyInput so simulator finger drags draw; `-pencilOnly 1` switches
        // to the production policy for the hardware spike run.
        canvas.drawingPolicy =
            UserDefaults.standard.bool(forKey: "pencilOnly") ? .pencilOnly : .anyInput
        canvas.tool = PKInkingTool(.pen, color: .black, width: 10)
        canvas.delegate = context.coordinator
        canvas.isAccessibilityElement = true
        canvas.accessibilityIdentifier = "spikeCanvas"

        let observer = TouchObserverGestureRecognizer(target: nil, action: nil)
        let coordinator = context.coordinator
        observer.onTouches = { phase, touches, event in
            coordinator.observed(phase, touches, event)
        }
        canvas.addGestureRecognizer(observer)

        let active = ActiveObserverGestureRecognizer(target: nil, action: nil)
        active.onTouches = { phase, touches, event in
            coordinator.observedActive(phase, touches, event)
        }
        canvas.addGestureRecognizer(active)
        return canvas
    }

    func updateUIView(_ canvas: PKCanvasView, context: Context) {}

    @MainActor
    final class Coordinator: NSObject, PKCanvasViewDelegate {
        let model: SpikeModel
        private var activeTouches = 0
        private var roundtrippedStrokes = 0

        init(model: SpikeModel) { self.model = model }

        func observed(_ phase: UITouch.Phase, _ touches: Set<UITouch>, _ event: UIEvent) {
            switch phase {
            case .began:
                activeTouches += touches.count
                model.began += touches.count
                NSLog("IM2 observer began touches=%d", touches.count)
            case .moved:
                model.moved += touches.count
                for touch in touches {
                    model.coalesced += event.coalescedTouches(for: touch)?.count ?? 0
                }
            case .ended, .cancelled:
                activeTouches -= min(activeTouches, touches.count)
                model.ended += touches.count
                NSLog("IM2 observer %@", phase == .ended ? "ended" : "cancelled")
            default:
                break
            }
        }

        func observedActive(_ phase: UITouch.Phase, _ touches: Set<UITouch>, _ event: UIEvent) {
            switch phase {
            case .began:
                model.activeBegan += touches.count
            case .moved:
                model.activeMoved += touches.count
            case .ended, .cancelled:
                model.activeEnded += touches.count
                NSLog("IM2 active-observer %@", phase == .ended ? "ended" : "cancelled")
            default:
                break
            }
        }

        nonisolated func canvasViewDrawingDidChange(_ canvasView: PKCanvasView) {
            MainActor.assumeIsolated {
                let drawing = canvasView.drawing
                model.didChangeCount += 1
                if activeTouches > 0 { model.midStrokeDidChange = true }
                model.strokes = drawing.strokes.count
                NSLog(
                    "IM2 didChange strokes=%d activeTouches=%d",
                    drawing.strokes.count, activeTouches)
                if drawing.strokes.count > roundtrippedStrokes,
                    let last = drawing.strokes.last
                {
                    roundtrippedStrokes = drawing.strokes.count
                    roundtrip(last)
                }
            }
        }

        /// (c): render the committed stroke against two rebuilds.
        private func roundtrip(_ stroke: PKStroke) {
            let original = PKDrawing(strokes: [stroke])
            let full = PKDrawing(strokes: [rebuildFull(stroke)])
            let schema = PKDrawing(strokes: [rebuildViaSchema(stroke)])
            model.maxFull = maxPixelDiff(original, full)
            model.maxSchema = maxPixelDiff(original, schema)
            NSLog(
                "IM2 roundtrip points=%d maxFull=%d maxSchema=%d",
                stroke.path.count, model.maxFull, model.maxSchema)
        }

        /// Rebuild copying every PKStrokePoint field verbatim.
        private func rebuildFull(_ stroke: PKStroke) -> PKStroke {
            let points = stroke.path.map { $0 }
            let path = PKStrokePath(controlPoints: points, creationDate: stroke.path.creationDate)
            return PKStroke(ink: stroke.ink, path: path, transform: stroke.transform, mask: stroke.mask)
        }

        /// Rebuild after squeezing each control point through the CRDT
        /// schema: PendantCore.StrokePoint (all f32, time in whole ms) with
        /// the per-point size channel added after the first hardware run
        /// showed dropping it costs up to 255/255 pixel diff.
        private func rebuildViaSchema(_ stroke: PKStroke) -> PKStroke {
            let baseWidth = Float(stroke.path.first?.size.width ?? 10)
            let baseHeight = Float(stroke.path.first?.size.height ?? 10)
            let opacity = stroke.path.first?.opacity ?? 1
            let encoded = stroke.path.map { p in
                StrokePoint(
                    x: Float(p.location.x),
                    y: Float(p.location.y),
                    force: Float(p.force),
                    tMs: UInt32(max(0, p.timeOffset * 1000)),
                    tilt: Tilt(azimuth: Float(p.azimuth), altitude: Float(p.altitude)),
                    size: PointSize(w: Float(p.size.width), h: Float(p.size.height)))
            }
            let rebuilt = encoded.map { q in
                PKStrokePoint(
                    location: CGPoint(x: CGFloat(q.x), y: CGFloat(q.y)),
                    timeOffset: TimeInterval(q.tMs) / 1000,
                    size: q.size.map { CGSize(width: CGFloat($0.w), height: CGFloat($0.h)) }
                        ?? CGSize(width: CGFloat(baseWidth), height: CGFloat(baseHeight)),
                    opacity: opacity,
                    force: CGFloat(q.force),
                    azimuth: CGFloat(q.tilt?.azimuth ?? 0),
                    altitude: CGFloat(q.tilt?.altitude ?? 0))
            }
            let path = PKStrokePath(controlPoints: rebuilt, creationDate: stroke.path.creationDate)
            return PKStroke(ink: stroke.ink, path: path)
        }

        private func maxPixelDiff(_ a: PKDrawing, _ b: PKDrawing) -> Int {
            let bounds = a.bounds.union(b.bounds).insetBy(dx: -10, dy: -10)
            guard bounds.width > 0, bounds.height > 0 else { return -1 }
            let pa = rgba(a.image(from: bounds, scale: 1))
            let pb = rgba(b.image(from: bounds, scale: 1))
            guard pa.count == pb.count, !pa.isEmpty else { return 255 }
            var worst = 0
            for i in 0..<pa.count {
                worst = max(worst, abs(Int(pa[i]) - Int(pb[i])))
            }
            return worst
        }

        private func rgba(_ image: UIImage) -> [UInt8] {
            guard let cg = image.cgImage else { return [] }
            let w = cg.width
            let h = cg.height
            var buffer = [UInt8](repeating: 0, count: w * h * 4)
            buffer.withUnsafeMutableBytes { raw in
                guard
                    let ctx = CGContext(
                        data: raw.baseAddress, width: w, height: h,
                        bitsPerComponent: 8, bytesPerRow: w * 4,
                        space: CGColorSpaceCreateDeviceRGB(),
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
                else { return }
                ctx.draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
            }
            return buffer
        }
    }
}
