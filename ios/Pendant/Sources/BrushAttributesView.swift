// The custom-brush popover above PencilKit's own colour and width
// controls: the handful of numbers the core exposes as `BrushKnobs`,
// written back into the brush's spec and remembered per brush.

import PendantCore
import SwiftUI

struct BrushAttributesView: View {
    let brush: LibraryBrush
    @State private var knobs: BrushKnobs
    @State private var newName = ""
    @State private var saved = false

    init(brush: LibraryBrush) {
        self.brush = brush
        let base = brushKnobs(spec: brush.spec)
        _knobs = State(
            initialValue: BrushKnobsStore.load(id: brush.id) ?? base
                ?? BrushKnobs(
                    opacity: 1, hardness: 1, spacing: nil, scatter: nil, sizeJitter: nil,
                    opacityJitter: nil, grainStrength: nil, grainScale: nil))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(brush.name).font(.headline)
            knob("opacity", $knobs.opacity, 0...1)
            knob("hardness", $knobs.hardness, 0...1)
            optional("spacing", $knobs.spacing, 0.02...1)
            optional("scatter", $knobs.scatter, 0...0.5)
            optional("size jitter", $knobs.sizeJitter, 0...1)
            optional("opacity jitter", $knobs.opacityJitter, 0...1)
            optional("grain", $knobs.grainStrength, 0...1)
            optional("grain scale", $knobs.grainScale, 0.5...8)
            HStack {
                Button("Reset") {
                    BrushKnobsStore.reset(id: brush.id)
                    if let base = brushKnobs(spec: brush.spec) { knobs = base }
                }
                Spacer()
                if BrushLibrary.isShared(brush.id) {
                    Button("Delete from library", role: .destructive) {
                        try? BrushLibrary.shared.remove(id: brush.id)
                    }
                }
            }
            .font(.caption)
            Divider()
            HStack {
                TextField("New brush name", text: $newName)
                    .textFieldStyle(.roundedBorder)
                    .font(.caption)
                Button(saved ? "Saved" : "Save to library") {
                    let spec = brushWithKnobs(spec: brush.spec, knobs: knobs)
                    let name = newName.isEmpty ? "\(brush.name) copy" : newName
                    saved = (try? BrushLibrary.shared.save(name: name, spec: spec)) != nil
                }
                .font(.caption)
                .disabled(saved)
            }
            Text("Library brushes appear in every device's picker after the sketch is reopened.")
                .font(.caption2).foregroundStyle(.secondary)
        }
        .padding(12)
        .onChange(of: knobsKey) { _, _ in BrushKnobsStore.save(id: brush.id, knobs: knobs) }
    }

    /// `BrushKnobs` is not `Equatable` across the FFI; watch its numbers.
    private var knobsKey: [Float?] {
        [
            knobs.opacity, knobs.hardness, knobs.spacing, knobs.scatter, knobs.sizeJitter,
            knobs.opacityJitter, knobs.grainStrength, knobs.grainScale,
        ]
    }

    private func knob(_ name: String, _ value: SwiftUI.Binding<Float>, _ range: ClosedRange<Float>)
        -> some View
    {
        HStack {
            Text(name).font(.caption).frame(width: 90, alignment: .leading)
            Slider(value: value, in: range)
            Text(String(format: "%.2f", value.wrappedValue))
                .font(.system(.caption, design: .monospaced)).frame(width: 40)
        }
    }

    @ViewBuilder
    private func optional(_ name: String, _ value: SwiftUI.Binding<Float?>, _ range: ClosedRange<Float>)
        -> some View
    {
        if let current = value.wrappedValue {
            knob(
                name,
                SwiftUI.Binding(get: { value.wrappedValue ?? current }, set: { value.wrappedValue = $0 }),
                range)
        }
    }
}
