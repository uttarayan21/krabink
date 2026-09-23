// `-brushLab 1`: tuning surface for the brush engine, no note or sync.
//
// Presets: every core brush drawn by `InkRenderer` from one canned stroke
// (a sine sweep with rising force and flattening tilt) at three widths, so
// a change to a preset or the renderer is judged side by side.
// Calibration: PencilKit draws the same stroke with its own ink above ours
// with the same tool, for the width/opacity ratios in `StrokeCodec`
// (`widthScale`, `opacityScale`): 1.0 until measured here.
// Corpus: the recordings bundled from `crates/krabink-core/tests/corpus/
// brush/` replayed through every input model this build has (EMA, and
// ISM when the core was built with the `ism` feature), the raw pen path
// in grey under each, with the corpus metrics; the desktop's
// `krabink brush-lab` draws the same grid from the same core.

import MetalKit
import PencilKit
import KrabinkCore
import SwiftUI
import UIKit

struct BrushLabScreen: View {
    /// `-labPage 0|1|2` and `-labModel ema|ism` preselect (screenshots,
    /// UI tests).
    @State private var page = UserDefaults.standard.integer(forKey: "labPage")
    @State private var model: InputModel =
        UserDefaults.standard.string(forKey: "labModel") == "ism" && ismAvailable() ? .ism : .ema

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Picker("page", selection: $page) {
                    Text("presets").tag(0)
                    Text("calibration").tag(1)
                    Text("corpus").tag(2)
                }
                .pickerStyle(.segmented)
                if page == 0 && ismAvailable() {
                    Picker("model", selection: $model) {
                        ForEach(LabStroke.models, id: \.self) { m in
                            Text(LabStroke.name(m)).tag(m)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(width: 140)
                    .accessibilityIdentifier("labModel")
                }
            }
            .padding(8)
            switch page {
            case 0:
                LabCanvas(elements: LabStroke.presetGrid(model: model))
                    .id(model)
            case 1:
                BrushCalibration()
            default:
                CorpusReplay()
            }
        }
    }
}

/// The canned stroke and the elements the lab draws from it.
enum LabStroke {
    /// Rows of the grid: the tool presets, then the bundled brushes
    /// (paired with the PencilKit ink the calibration page compares to).
    static let presets: [(Tool, PKInkingTool.InkType, CustomBrush?)] = {
        let custom = { (id: String) -> CustomBrush? in
            StrokeCodec.builtins[id].map { CustomBrush(id: $0.id, spec: $0.spec) }
        }
        return [
            (.pen, .pen, nil), (.pencil, .pencil, nil), (.marker, .marker, nil),
            (.monoline, .monoline, nil), (.fountain, .fountainPen, nil),
            (.pencil, .crayon, custom("builtin:crayon")),
            (.pencil, .pencil, custom("builtin:pencil-grainy")),
            (.pencil, .pencil, custom("builtin:chalk")),
        ]
    }()

    /// The row's label: the tool, or the bundled brush's id.
    static func name(_ row: Int) -> String {
        presets[row].2?.id ?? String(describing: presets[row].0)
    }
    static let widths: [Float] = [3, 8, 16]
    static let length: Float = 320
    static let rowHeight: CGFloat = 70
    static let columnWidth: CGFloat = 380

    /// 160 samples at 240 Hz: a sine sweep, force 0.2 → 0.9, altitude
    /// 0.9 → 0.5 rad with a fixed azimuth so nibs and pencils show tilt.
    static func samples(origin: CGPoint) -> [RawSample] {
        (0...160).map { i in
            let t = Float(i) / 160
            return RawSample(
                x: Float(origin.x) + t * length,
                y: Float(origin.y) + 18 * sin(t * 4 * .pi),
                force: 0.2 + 0.7 * t, tMs: Double(i) * 4.17,
                tilt: Tilt(azimuth: 0.6, altitude: 0.9 - 0.4 * t, roll: 0),
                estimationId: nil, expectsUpdate: false)
        }
    }

    /// Valid ULID text for lab-only elements (Crockford base32).
    static func id(_ n: Int) -> String {
        let alphabet = Array("0123456789ABCDEFGHJKMNPQRSTVWXYZ")
        return "01ARZ3NDEKTSV4RRFFQ69G5F" + String(alphabet[n / 32 % 32]) + String(alphabet[n % 32])
    }

    /// The input models this build offers: EMA always, ISM with the
    /// `ism` core feature.
    static var models: [InputModel] { ismAvailable() ? [.ema, .ism] : [.ema] }

    static func name(_ model: InputModel) -> String {
        switch model {
        case .ema: "EMA"
        case .ism: "ISM"
        }
    }

    /// A modeler for `brush` running `model`, the EMA one when the build
    /// lacks the model asked for.
    static func modeler(_ brush: BrushRef, model: InputModel) -> BrushModeler {
        (try? BrushModeler.forBrushWith(brush: brush, model: model)) ?? BrushModeler.forBrush(brush: brush)
    }

    static func stroke(
        _ n: Int, tool: Tool, custom: CustomBrush? = nil, width: Float, color: UInt32, origin: CGPoint,
        model: InputModel = .ema
    ) -> Stroke {
        let modeler = modeler(BrushRef(tool: tool, baseWidth: width, custom: custom), model: model)
        _ = modeler.push(samples: samples(origin: origin))
        return Stroke(
            id: id(n), tool: tool, color: color, baseWidth: width, kind: .polylineSample,
            points: modeler.finish(), createdMs: 0, brush: custom)
    }

    /// Rows: presets; columns: widths. A translucent blue bar under every
    /// row shows blend and self-overlap behaviour.
    static func presetGrid(model: InputModel = .ema) -> [Element] {
        var out: [Element] = []
        for (row, (tool, _, custom)) in presets.enumerated() {
            let y = 40 + CGFloat(row) * rowHeight
            for (col, width) in widths.enumerated() {
                let x = 20 + CGFloat(col) * columnWidth
                out.append(
                    .stroke(
                        stroke(
                            out.count, tool: tool, custom: custom, width: width, color: 0x1E3CC8FF,
                            origin: CGPoint(x: x, y: y), model: model)))
            }
        }
        return out
    }
}

/// One bundled recording replayed through every input model.
struct CorpusReplay: View {
    @State private var files: [URL] = CorpusReplay.bundled()
    @State private var index = 0

    var body: some View {
        VStack(spacing: 4) {
            if files.isEmpty {
                Text("no recordings bundled (Krabink/corpus)").padding()
            } else {
                Picker("recording", selection: $index) {
                    ForEach(files.indices, id: \.self) { i in
                        Text(files[i].deletingPathExtension().lastPathComponent).tag(i)
                    }
                }
                .pickerStyle(.menu)
                .accessibilityIdentifier("labRecording")
                let replay = CorpusReplay.replay(files[index])
                Text(replay.summary)
                    .font(.system(.caption, design: .monospaced))
                    .accessibilityIdentifier("labMetrics")
                LabCanvas(elements: replay.elements).id(index)
            }
        }
    }

    /// The `*.txt` recordings copied into the bundle's `brush` folder.
    static func bundled() -> [URL] {
        (Bundle.main.urls(forResourcesWithExtension: "txt", subdirectory: "brush") ?? [])
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
    }

    struct Replay {
        var elements: [Element] = []
        var summary = ""
    }

    /// Rows: models, each the raw path in grey under the modelled stroke,
    /// stacked down the canvas. The summary is one metrics line per model.
    static func replay(_ url: URL) -> Replay {
        guard let text = try? String(contentsOf: url, encoding: .utf8),
            let rec = try? parseRecording(text: text)
        else {
            return Replay(summary: "unreadable recording")
        }
        let xs = rec.samples.map(\.x)
        let ys = rec.samples.map(\.y)
        let minX = xs.min() ?? 0, minY = ys.min() ?? 0, maxY = ys.max() ?? 0
        let pad: Float = 24
        let cellHeight = maxY - minY + 2 * pad
        let brush = BrushRef(tool: rec.tool, baseWidth: rec.size)
        var out = Replay()
        var lines: [String] = []
        for (row, model) in LabStroke.models.enumerated() {
            let shift = SIMD2<Float>(pad - minX, Float(row) * cellHeight + pad - minY)
            let origin = rec.samples.first?.tMs ?? 0
            let raw = rec.samples.map { s in
                StrokePoint(
                    x: s.x + shift.x, y: s.y + shift.y, force: 1,
                    tMs: UInt32(max(0, (s.tMs - origin).rounded())), tilt: nil, size: nil)
            }
            out.elements.append(
                .stroke(
                    Stroke(
                        id: LabStroke.id(out.elements.count), tool: .monoline, color: 0xA0A0A0FF,
                        baseWidth: 0.6, kind: .polylineSample, points: raw, createdMs: 0, brush: nil)))
            let modeler = LabStroke.modeler(brush, model: model)
            _ = modeler.push(samples: rec.samples)
            let points = modeler.finish().map { p in
                StrokePoint(x: p.x + shift.x, y: p.y + shift.y, force: p.force, tMs: p.tMs, tilt: p.tilt, size: p.size)
            }
            out.elements.append(
                .stroke(
                    Stroke(
                        id: LabStroke.id(out.elements.count), tool: rec.tool,
                        color: rec.color ?? 0x000000FF, baseWidth: rec.size, kind: .polylineSample,
                        points: points, createdMs: 0, brush: nil)))
            if let m = try? measureRecording(recording: rec, brush: brush, model: model) {
                lines.append(
                    String(
                        format: "%@ points=%d jitter=%.3f lag=%.2f dev=%.2f over=%.3f %dµs",
                        LabStroke.name(model), m.points, m.jitter, m.lag, m.deviation, m.overshoot, m.micros))
            }
        }
        out.summary = lines.joined(separator: "\n")
        return out
    }
}

/// A sketch canvas showing fixed elements (pan and zoom still work).
struct LabCanvas: UIViewRepresentable {
    let elements: [Element]

    func makeUIView(context: Context) -> UIView {
        guard let canvas = SketchCanvasView(policy: .anyInput) else {
            let label = UILabel()
            label.text = "Metal is unavailable"
            return label
        }
        for (z, element) in elements.enumerated() { canvas.renderer.show(element, z: z) }
        return canvas
    }

    func updateUIView(_ view: UIView, context: Context) {}
}

/// PencilKit above, ours below, same stroke, same tool.
struct BrushCalibration: View {
    @State private var preset = 0
    @State private var width: Float = 8

    var body: some View {
        VStack(spacing: 4) {
            HStack {
                Picker("ink", selection: $preset) {
                    ForEach(LabStroke.presets.indices, id: \.self) { i in
                        Text(LabStroke.name(i)).tag(i)
                    }
                }
                .pickerStyle(.menu)
                Slider(value: $width, in: 1...32, step: 1)
                Text(String(format: "w=%.0f", width)).font(.system(.caption, design: .monospaced))
            }
            .padding(.horizontal, 8)
            PencilKitReference(ink: LabStroke.presets[preset].1, width: CGFloat(width))
                .frame(height: 140)
            LabCanvas(elements: [
                .stroke(LabStroke.stroke(
                    0, tool: LabStroke.presets[preset].0, custom: LabStroke.presets[preset].2,
                    width: width, color: 0x000000FF,
                    origin: CGPoint(x: 20, y: 70))),
            ])
            .id("\(preset)-\(width)")
        }
    }
}

/// PencilKit's rendering of the canned stroke with its own ink.
struct PencilKitReference: UIViewRepresentable {
    let ink: PKInkingTool.InkType
    let width: CGFloat

    func makeUIView(context: Context) -> PKCanvasView {
        let view = PKCanvasView()
        view.isUserInteractionEnabled = false
        view.backgroundColor = .systemBackground
        return view
    }

    func updateUIView(_ view: PKCanvasView, context: Context) {
        let samples = LabStroke.samples(origin: CGPoint(x: 20, y: 70))
        let points = samples.map { s in
            PKStrokePoint(
                location: CGPoint(x: CGFloat(s.x), y: CGFloat(s.y)),
                timeOffset: s.tMs / 1000, size: CGSize(width: width, height: width),
                opacity: 1, force: CGFloat(s.force) * 2,
                azimuth: CGFloat(s.tilt?.azimuth ?? 0), altitude: CGFloat(s.tilt?.altitude ?? .pi / 2))
        }
        let path = PKStrokePath(controlPoints: points, creationDate: Date())
        let stroke = PKStroke(ink: PKInk(ink, color: .black), path: path)
        view.drawing = PKDrawing(strokes: [stroke])
    }
}
