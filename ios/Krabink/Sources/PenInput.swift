// Pen input for the note canvas: which touches ink, how a touch becomes a
// `RawSample` for the core's `BrushModeler`, the gesture recognizer that
// captures pen touches with their coalesced and predicted siblings, and
// the `-recordStrokes 1` recorder for the shape corpus.
//
// The recognizer sits on the editor's text view. A Pencil touch is ink
// the moment it lands: the recognizer begins at once, cancels the touch
// for the view and, through the delegate, makes the text view's own
// recognizers (taps, long press, selection) wait for it and fail. Under
// `.anyInput` a finger may ink too, but a short, still touch is a tap for
// the caret: the recognizer holds the first samples until the touch moved
// `tapSlop` points or lasted `tapDelay`, and fails instead when the touch
// ends before that, so the text view's tap lands the caret.

import KrabinkCore
import UIKit

/// Pencil force in Apple's units is 1 at average pressure and can reach
/// about 4; full ink width sits at twice average so hard pressure has room
/// to show. Fingers report no force and get the nominal 0.5.
private let forceFullScale: CGFloat = 2
/// `-fakeEstimates 1`: finger samples pretend to be estimates that the
/// model revises 60 ms after pen-up, so the settling path (ink held on
/// screen, points patched, early commit) runs on the simulator.
let fakeEstimates = UserDefaults.standard.bool(forKey: "fakeEstimates")
/// A direct touch that ends within this distance and time is a tap, not
/// a stroke.
private let tapSlop: CGFloat = 4
private let tapDelay: Duration = .milliseconds(250)

/// Which touches ink. The Pencil only on device (fingers scroll and place
/// the caret); fingers too on the simulator and under `-anyInput 1`, so
/// UI tests can draw with drags. `-anyInput 0` forces pencil-only anywhere.
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
    /// One touch as the modeler sees it, in `view`'s coordinates (the
    /// text view's content space). Tilt is captured for every pencil touch
    /// (barrel roll is Apple Pencil Pro only, iOS 17.5+, 0 otherwise).
    /// Force and tilt may still be estimates: `estimationId` names the
    /// touch so the update can be patched in through `BrushModeler.update`.
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
        var estimationId = touch.estimationUpdateIndex?.uint32Value
        var expectsUpdate = !expecting.intersection([.force, .azimuth, .altitude]).isEmpty
        if fakeEstimates, !pencil {
            estimationId = UInt32(truncatingIfNeeded: Int(touch.timestamp * 1_000_000))
            expectsUpdate = true
        }
        self.init(
            x: Float(location.x), y: Float(location.y),
            force: pencil ? Float(min(touch.force / forceFullScale, 1)) : 0.5,
            tMs: touch.timestamp * 1000,
            tilt: tilt,
            estimationId: estimationId,
            expectsUpdate: expectsUpdate)
    }

    /// The same sample moved by `-origin`: content space to anchor space.
    func translated(by origin: CGPoint) -> RawSample {
        var out = self
        out.x -= Float(origin.x)
        out.y -= Float(origin.y)
        return out
    }
}

/// Captures pen (or, under `.anyInput`, finger) touches with the full
/// `UIEvent`, which is where coalesced and predicted touches live. Tracks
/// one touch and cancels the stroke when a second one lands (a two-finger
/// scroll, not ink).
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
    /// Touch locations are taken in this view (the text view: its bounds
    /// are content coordinates).
    weak var canvasSpace: UIView?
    private var tracked: UITouch?
    /// A direct touch not yet known to be a stroke: its samples so far,
    /// where and when it landed.
    private var pending: (samples: [RawSample], at: CGPoint, since: TimeInterval)?
    private var pendingTask: Task<Void, Never>?

    init(policy: InputPolicy) {
        super.init(target: nil, action: nil)
        allowedTouchTypes = policy.touchTypes
        // Once this recognizes, the text view never sees the touch: no
        // caret, no magnifier, no selection under a stroke.
        cancelsTouchesInView = true
        // A finger that turns out to be a tap is handed to the view late
        // (≤ `tapDelay`), a stroke never.
        delaysTouchesBegan = policy == .anyInput
        delaysTouchesEnded = false
        delegate = self
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        if tracked != nil {
            // A second finger: a scroll, not a stroke.
            cancelStroke()
            return
        }
        guard let touch = touches.first, let space = canvasSpace else { return }
        tracked = touch
        let sample = RawSample(touch, in: space)
        if touch.type == .pencil {
            onPhase?(.began(sample))
            state = .began
            return
        }
        // A finger: could still be a tap for the caret.
        pending = ([sample], touch.location(in: space), touch.timestamp)
        pendingTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: tapDelay)
            guard !Task.isCancelled else { return }
            self?.commitPending()
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        guard let touch = tracked, touches.contains(touch), let space = canvasSpace else { return }
        let coalesced = (event.coalescedTouches(for: touch) ?? [touch]).map {
            RawSample($0, in: space)
        }
        if pending != nil {
            pending!.samples.append(contentsOf: coalesced)
            let moved = touch.location(in: space)
            if hypot(moved.x - pending!.at.x, moved.y - pending!.at.y) >= tapSlop {
                commitPending()
            }
            return
        }
        let predicted = (event.predictedTouches(for: touch) ?? []).map { RawSample($0, in: space) }
        onPhase?(.moved(coalesced: coalesced, predicted: predicted))
        state = .changed
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        guard let touch = tracked, touches.contains(touch) else { return }
        if pending != nil {
            // Ended before it became a stroke: a tap, the text view's.
            dropPending()
            tracked = nil
            state = .failed
            return
        }
        finish(.ended)
        state = .ended
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        guard let touch = tracked, touches.contains(touch) else { return }
        cancelStroke()
    }

    /// Arrives for pencil touches while and after the stroke, until every
    /// estimated property settled; not gated on `tracked`.
    override func touchesEstimatedPropertiesUpdated(_ touches: Set<UITouch>) {
        guard let space = canvasSpace else { return }
        onPhase?(.estimateUpdated(touches.map { RawSample($0, in: space) }))
    }

    override func reset() {
        tracked = nil
        dropPending()
    }

    /// The held finger samples become a stroke.
    private func commitPending() {
        guard let held = pending else { return }
        dropPending()
        onPhase?(.began(held.samples[0]))
        state = .began
        if held.samples.count > 1 {
            onPhase?(.moved(coalesced: Array(held.samples.dropFirst()), predicted: []))
            state = .changed
        }
    }

    private func dropPending() {
        pendingTask?.cancel()
        pendingTask = nil
        pending = nil
    }

    private func cancelStroke() {
        if pending != nil {
            dropPending()
            tracked = nil
            state = .failed
            return
        }
        finish(.cancelled)
        state = .cancelled
    }

    private func finish(_ phase: Phase) {
        tracked = nil
        onPhase?(phase)
    }

    // MARK: arbitration with the text view

    /// Two-finger scrolling (the text view's pan) may run alongside a
    /// stroke; the stroke cancels itself when the second finger lands.
    func gestureRecognizer(
        _ gestureRecognizer: UIGestureRecognizer,
        shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer
    ) -> Bool {
        other is UIPanGestureRecognizer
    }

    /// Everything else on the text view (tap for the caret, long press,
    /// selection) waits for this recognizer: a Pencil begins at once so
    /// they fail; a finger tap fails this one so they proceed.
    func gestureRecognizer(
        _ gestureRecognizer: UIGestureRecognizer,
        shouldBeRequiredToFailBy other: UIGestureRecognizer
    ) -> Bool {
        !(other is UIPanGestureRecognizer) && other.view === gestureRecognizer.view
    }
}

/// Writes raw pen samples to Documents for the core's shape corpus
/// (`crates/krabink-core/tests/corpus/shapes`), one file per stroke in the
/// replay format. Enabled by the `-recordStrokes 1` launch argument; the
/// app shares Documents with Files so the recordings can be copied out.
enum StrokeRecorder {
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
        var text = "# krabink-stroke v2\n# expect: none\n"
        text += "# tool: \(selection.brush.tool) size: \(selection.brush.baseWidth)"
        text += String(
            format: " color: %08x brush: %@\n", selection.color,
            selection.brush.custom?.id ?? "preset")
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
