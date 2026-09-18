// `-brushLab 1`: tuning surface for the brush engine, no note or sync.
//
// Presets: every core brush drawn by `InkRenderer` from one canned stroke
// (a sine sweep with rising force and flattening tilt) at three widths, so
// a change to a preset or the renderer is judged side by side.
// Calibration: PencilKit draws the same stroke with its own ink above ours
// with the same tool, for the width/opacity ratios in `StrokeCodec`
// (`widthScale`, `opacityScale`): 1.0 until measured here.

import MetalKit
import PencilKit
import PendantCore
import SwiftUI
import UIKit

struct BrushLabScreen: View {
    @State private var page = 0

    var body: some View {
        VStack(spacing: 0) {
            Picker("page", selection: $page) {
                Text("presets").tag(0)
                Text("calibration").tag(1)
            }
            .pickerStyle(.segmented)
            .padding(8)
            if page == 0 {
                LabCanvas(elements: LabStroke.presetGrid())
            } else {
                BrushCalibration()
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

    static func stroke(
        _ n: Int, tool: Tool, custom: CustomBrush? = nil, width: Float, color: UInt32, origin: CGPoint
    ) -> Stroke {
        let modeler = BrushModeler.forBrush(brush: BrushRef(tool: tool, baseWidth: width, custom: custom))
        _ = modeler.push(samples: samples(origin: origin))
        return Stroke(
            id: id(n), tool: tool, color: color, baseWidth: width, kind: .polylineSample,
            points: modeler.finish(), createdMs: 0, brush: custom)
    }

    /// Rows: presets; columns: widths. A translucent blue bar under every
    /// row shows blend and self-overlap behaviour.
    static func presetGrid() -> [Element] {
        var out: [Element] = []
        for (row, (tool, _, custom)) in presets.enumerated() {
            let y = 40 + CGFloat(row) * rowHeight
            for (col, width) in widths.enumerated() {
                let x = 20 + CGFloat(col) * columnWidth
                out.append(
                    .stroke(
                        stroke(
                            out.count, tool: tool, custom: custom, width: width, color: 0x1E3CC8FF,
                            origin: CGPoint(x: x, y: y))))
            }
        }
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
