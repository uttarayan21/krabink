// The custom-brush popover above PencilKit's own colour and width
// controls: the handful of numbers the core exposes as `BrushKnobs`,
// written back into the brush's spec and remembered per brush.

import PendantCore
import SwiftUI
import UniformTypeIdentifiers

struct BrushAttributesView: View {
    let brush: LibraryBrush
    @State private var knobs: BrushKnobs
    @State private var newName = ""
    @State private var saved = false
    /// Asset id, or "" for the plain shape / procedural noise.
    @State private var maskAsset: String
    @State private var grainAsset: String
    @State private var importing: BrushKnobsStore.ImageSlot?
    @State private var importError: String?

    init(brush: LibraryBrush) {
        self.brush = brush
        let base = brushKnobs(spec: brush.spec)
        _knobs = State(
            initialValue: BrushKnobsStore.load(id: brush.id) ?? base
                ?? BrushKnobs(
                    opacity: 1, hardness: 1, spacing: nil, scatter: nil, sizeJitter: nil,
                    opacityJitter: nil, grainStrength: nil, grainScale: nil))
        // What the spec samples today, unless the user chose otherwise.
        let inSpec = brushAssets(spec: brush.spec)
        let assets = InkAssets.shared.assets
        let specMask = inSpec.first { id in assets.contains { $0.id == id && $0.kind == .mask } }
        let specGrain = inSpec.first { id in assets.contains { $0.id == id && $0.kind == .grain } }
        _maskAsset = State(
            initialValue: BrushKnobsStore.loadImage(id: brush.id, slot: .mask).map { $0 ?? "" }
                ?? specMask ?? "")
        _grainAsset = State(
            initialValue: BrushKnobsStore.loadImage(id: brush.id, slot: .grain).map { $0 ?? "" }
                ?? specGrain ?? "")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(brush.name).font(.headline)
            imagePicker("tip mask", $maskAsset, kind: .mask, none: "shape", slot: .mask)
            imagePicker("paper", $grainAsset, kind: .grain, none: "noise", slot: .grain)
            if let importError {
                Text(importError).font(.caption2).foregroundStyle(.red)
            }
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
        .onChange(of: maskAsset) { _, v in
            BrushKnobsStore.saveImage(id: brush.id, slot: .mask, asset: v.isEmpty ? nil : v)
        }
        .onChange(of: grainAsset) { _, v in
            BrushKnobsStore.saveImage(id: brush.id, slot: .grain, asset: v.isEmpty ? nil : v)
        }
        .fileImporter(
            isPresented: Binding(get: { importing != nil }, set: { if !$0 { importing = nil } }),
            allowedContentTypes: [.image]
        ) { result in
            guard let slot = importing else { return }
            importing = nil
            switch result {
            case .success(let url):
                importPicked(url, slot: slot)
            case .failure(let err):
                importError = err.localizedDescription
            }
        }
    }

    /// Shrink the picked picture to a greyscale PNG the workspace accepts
    /// and select it.
    private func importPicked(_ url: URL, slot: BrushKnobsStore.ImageSlot) {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        guard let data = try? Data(contentsOf: url), let image = UIImage(data: data) else {
            importError = "could not read the picture"
            return
        }
        let kind: AssetKind = slot == .mask ? .mask : .grain
        do {
            guard let id = try BrushLibrary.shared.importAsset(image, name: url.lastPathComponent, kind: kind)
            else {
                importError = "picture did not fit in 64 KiB"
                return
            }
            importError = nil
            if slot == .mask { maskAsset = id } else { grainAsset = id }
        } catch {
            importError = "\(error)"
        }
    }

    /// A menu of the assets of one kind, the built-in option first, and an
    /// import entry that files a picture into the workspace.
    private func imagePicker(
        _ name: String, _ selection: SwiftUI.Binding<String>, kind: AssetKind, none: String,
        slot: BrushKnobsStore.ImageSlot
    ) -> some View {
        HStack {
            Text(name).font(.caption).frame(width: 90, alignment: .leading)
            Picker(name, selection: selection) {
                Text(none).tag("")
                ForEach(InkAssets.shared.assets.filter { $0.kind == kind }, id: \.id) { asset in
                    Text(asset.name).tag(asset.id)
                }
            }
            .pickerStyle(.menu)
            .labelsHidden()
            Spacer()
            Button("Import…") { importing = slot }.font(.caption)
        }
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
