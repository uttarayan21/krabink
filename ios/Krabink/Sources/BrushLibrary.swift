// The brushes the tool picker offers beyond PencilKit's own inks: the
// brushes bundled with the app (`builtin:` ids from the core) and the
// workspace's shared library. Each is a `PKToolPickerCustomItem` on
// iOS 18 whose icon is the brush itself, drawn by the ink renderer;
// strokes snapshot the brush's spec so a document renders the same
// without this library.

import MetalKit
import PencilKit
import KrabinkCore
import SwiftUI
import UIKit

/// Per-brush knob overrides, persisted as JSON under `brush.<id>.knobs`.
enum BrushKnobsStore {
    private struct Stored: Codable {
        var opacity: Float
        var hardness: Float
        var spacing: Float?
        var scatter: Float?
        var sizeJitter: Float?
        var opacityJitter: Float?
        var grainStrength: Float?
        var grainScale: Float?
    }

    static func key(_ id: String) -> String { "brush.\(id).knobs" }

    static func load(id: String) -> BrushKnobs? {
        guard let data = UserDefaults.standard.data(forKey: key(id)),
            let s = try? JSONDecoder().decode(Stored.self, from: data)
        else { return nil }
        return BrushKnobs(
            opacity: s.opacity, hardness: s.hardness, spacing: s.spacing, scatter: s.scatter,
            sizeJitter: s.sizeJitter, opacityJitter: s.opacityJitter,
            grainStrength: s.grainStrength, grainScale: s.grainScale)
    }

    static func save(id: String, knobs: BrushKnobs) {
        let s = Stored(
            opacity: knobs.opacity, hardness: knobs.hardness, spacing: knobs.spacing,
            scatter: knobs.scatter, sizeJitter: knobs.sizeJitter,
            opacityJitter: knobs.opacityJitter, grainStrength: knobs.grainStrength,
            grainScale: knobs.grainScale)
        if let data = try? JSONEncoder().encode(s) {
            UserDefaults.standard.set(data, forKey: key(id))
        }
    }

    static func reset(id: String) {
        UserDefaults.standard.removeObject(forKey: key(id))
        UserDefaults.standard.removeObject(forKey: imageKey(id, .mask))
        UserDefaults.standard.removeObject(forKey: imageKey(id, .grain))
    }

    enum ImageSlot: String { case mask, grain }

    static func imageKey(_ id: String, _ slot: ImageSlot) -> String { "brush.\(id).\(slot.rawValue)" }

    /// `nil` when untouched; `.some(nil)` for "the plain shape / noise";
    /// `.some(id)` for an asset.
    static func loadImage(id: String, slot: ImageSlot) -> String?? {
        guard let stored = UserDefaults.standard.string(forKey: imageKey(id, slot)) else { return nil }
        return .some(stored.isEmpty ? nil : stored)
    }

    static func saveImage(id: String, slot: ImageSlot, asset: String?) {
        UserDefaults.standard.set(asset ?? "", forKey: imageKey(id, slot))
    }
}

/// A brush the picker can select by id.
struct LibraryBrush: Equatable {
    let id: String
    let name: String
    /// The preset tool strokes record alongside the spec.
    let tool: Tool
    /// Encoded `BrushSpec`.
    let spec: Data
}

@MainActor
final class BrushLibrary {
    static let shared = BrushLibrary()

    /// Bundled first, then the workspace's, newest edit first.
    private(set) var brushes: [LibraryBrush]
    private let builtins: [LibraryBrush]
    private weak var core: Core?

    /// The workspace whose library this is; reads it once, then follows
    /// `brushesChanged`.
    func attach(_ core: Core) {
        self.core = core
        setShared(core.listBrushes())
    }

    /// Is this a workspace brush (one the user can delete)?
    static func isShared(_ id: String) -> Bool { id.hasPrefix("user:") }

    /// Add `spec` to the workspace library under a fresh `user:` id; every
    /// device's picker offers it once its sketch is reopened.
    func save(name: String, spec: Data) throws {
        guard let core else { return }
        try core.upsertBrush(id: "user:\(UUID().uuidString.lowercased())", name: name, spec: spec)
        setShared(core.listBrushes())
    }

    func remove(id: String) throws {
        guard let core, Self.isShared(id) else { return }
        try core.removeBrush(id: id)
        BrushKnobsStore.reset(id: id)
        setShared(core.listBrushes())
    }

    init() {
        builtins = builtinBrushes().map {
            LibraryBrush(id: $0.id, name: $0.name, tool: .pencil, spec: $0.spec)
        }
        brushes = builtins
    }

    /// The brush under `id`, with the knobs and image choices the user
    /// set in its popover (`UserDefaults` `brush.<id>.*`) written into
    /// its spec.
    func brush(id: String) -> LibraryBrush? {
        guard let base = brushes.first(where: { $0.id == id }) else { return nil }
        var spec = base.spec
        if let knobs = BrushKnobsStore.load(id: id) { spec = brushWithKnobs(spec: spec, knobs: knobs) }
        if let mask = BrushKnobsStore.loadImage(id: id, slot: .mask) {
            spec = brushWithMask(spec: spec, asset: mask)
        }
        if let grain = BrushKnobsStore.loadImage(id: id, slot: .grain) {
            spec = brushWithGrainImage(spec: spec, asset: grain)
        }
        return LibraryBrush(id: base.id, name: base.name, tool: base.tool, spec: spec)
    }

    /// Add a picked picture to the workspace as a mask or grain; the
    /// asset id, or `nil` when it could not be made small enough.
    func importAsset(_ image: UIImage, name: String, kind: AssetKind) throws -> String? {
        guard let core, let png = AssetImport.greyscalePNG(image) else { return nil }
        let id = try core.putAsset(name: name, kind: kind, png: png)
        InkAssets.shared.setShared(core.listAssets())
        return id
    }

    /// The workspace library as the core reports it (`brushesChanged`).
    func setShared(_ shared: [BrushInfo]) {
        brushes =
            builtins
            + shared.map { LibraryBrush(id: $0.id, name: $0.name, tool: .pencil, spec: $0.spec) }
    }

    /// Picker items for every library brush that PencilKit has no ink
    /// for (the crayon rides on PencilKit's crayon item).
    @available(iOS 18.0, *)
    func pickerItems() -> [PKToolPickerCustomItem] {
        brushes.filter { $0.id != "builtin:crayon" }.map { brush in
            var config = PKToolPickerCustomItem.Configuration(identifier: brush.id, name: brush.name)
            config.allowsColorSelection = true
            config.defaultColor = .black
            config.defaultWidth = 8
            config.widthVariants = Dictionary(
                uniqueKeysWithValues: [3, 8, 16, 32].map { width in
                    (CGFloat(width), BrushIcon.dot(brush: brush, width: Float(width)))
                })
            config.imageProvider = { item in
                MainActor.assumeIsolated {
                    let current = BrushLibrary.shared.brush(id: brush.id) ?? brush
                    return BrushIcon.image(brush: current, color: item.color, width: Float(item.width))
                }
            }
            config.viewControllerProvider = { _ in
                MainActor.assumeIsolated {
                    let controller = UIHostingController(rootView: BrushAttributesView(brush: brush))
                    controller.preferredContentSize = CGSize(width: 320, height: 300)
                    return controller
                }
            }
            return PKToolPickerCustomItem(configuration: config)
        }
    }
}

/// Tool icons drawn with the brush itself, through the same core and
/// renderer as the canvas.
@MainActor
enum BrushIcon {
    private static let renderer = InkRenderer(
        view: MTKView(frame: .zero, device: MTLCreateSystemDefaultDevice()))

    /// The picker's tool image: a vertical stroke with a wobble, at least
    /// 150 pt tall as PencilKit asks.
    static func image(brush: LibraryBrush, color: UIColor, width: Float) -> UIImage {
        let ref = BrushRef(
            tool: brush.tool, baseWidth: max(2, min(width, 24)),
            custom: CustomBrush(id: brush.id, spec: brush.spec))
        let modeler = BrushModeler.forBrush(brush: ref)
        _ = modeler.push(
            samples: (0...40).map { i in
                let t = Float(i) / 40
                return RawSample(
                    x: 30 + 5 * sin(t * 2 * .pi), y: 10 + t * 140, force: 0.3 + 0.5 * t,
                    tMs: Double(i) * 8, tilt: nil, estimationId: nil, expectsUpdate: false)
            })
        return render(stroke(ref, color: color, points: modeler.finish()), maxSide: 160)
    }

    /// A width-variant swatch: one dab at that width, 32 pt square.
    static func dot(brush: LibraryBrush, width: Float) -> UIImage {
        let ref = BrushRef(
            tool: brush.tool, baseWidth: width, custom: CustomBrush(id: brush.id, spec: brush.spec))
        let modeler = BrushModeler.forBrush(brush: ref)
        _ = modeler.push(
            samples: (0...8).map { i in
                RawSample(
                    x: 20 + Float(i) * 0.5, y: 20, force: 0.6, tMs: Double(i) * 8, tilt: nil,
                    estimationId: nil, expectsUpdate: false)
            })
        return render(stroke(ref, color: .label, points: modeler.finish()), maxSide: 32)
    }

    private static func stroke(_ ref: BrushRef, color: UIColor, points: [StrokePoint]) -> Stroke {
        Stroke(
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", tool: ref.tool, color: StrokeCodec.pack(color),
            baseWidth: ref.baseWidth, kind: .polylineSample, points: points, createdMs: 0,
            brush: ref.custom)
    }

    private static func render(_ stroke: Stroke, maxSide: CGFloat) -> UIImage {
        renderer?.renderThumbnail(
            elements: [.stroke(stroke)], maxSide: maxSide, background: .clear, trait: .current)
            ?? UIImage()
    }
}
