// PencilKit tool picker ↔ core brush model. Only the picker survives from
// PencilKit: its ink type maps onto a core brush preset, its colour onto
// the packed RGBA the CRDT stores, its width onto the brush's base width.
// Stroke geometry never touches PencilKit.

import PencilKit
import PendantCore
import UIKit

/// What the pen draws with: a brush at a width, in a colour whose alpha
/// carries any opacity scaling (effective opacity = alpha × the preset's).
struct BrushSelection: Equatable {
    var brush: BrushRef
    /// 0xRRGGBBAA.
    var color: UInt32
}

extension BrushSelection {
    /// The same selection with a custom brush's spec re-read from the
    /// library, so edits made after the picker reported it apply.
    @MainActor
    func refreshed() -> BrushSelection {
        guard let custom = brush.custom, let current = BrushLibrary.shared.brush(id: custom.id)
        else { return self }
        var out = self
        out.brush.custom = CustomBrush(id: current.id, spec: current.spec)
        return out
    }
}

/// What the tool picker selected.
enum PickedTool {
    case ink(BrushSelection)
    case eraser(PKEraserTool)
}

enum StrokeCodec {
    /// Base width per PencilKit width unit. 1.0 until measured against
    /// PencilKit with the `-brushLab 1` calibration page.
    static let widthScale: [PKInkingTool.InkType: Float] = [
        .pen: 1, .pencil: 1, .marker: 1, .monoline: 1, .fountainPen: 1,
        // Crayon is a wide soft pencil until the stamped brush lands.
        .crayon: 1.5, .watercolor: 1,
    ]
    /// Alpha scale per PencilKit ink, on top of the preset's opacity.
    static let opacityScale: [PKInkingTool.InkType: Float] = [
        // Watercolour cannot be rendered honestly; it stores as a lighter marker.
        .watercolor: 0.6
    ]

    /// The brushes bundled with the app, by id.
    static let builtins: [String: BuiltinBrush] = Dictionary(
        uniqueKeysWithValues: builtinBrushes().map { ($0.id, $0) })

    /// PencilKit inks that draw with a bundled brush rather than a preset:
    /// the crayon is the stamped `builtin:crayon`, recorded as a pencil.
    static func custom(_ ink: PKInkingTool.InkType) -> CustomBrush? {
        let id: String
        switch ink {
        case .crayon: id = "builtin:crayon"
        default: return nil
        }
        guard let brush = builtins[id] else { return nil }
        return CustomBrush(id: brush.id, spec: brush.spec)
    }

    static func tool(_ ink: PKInkingTool.InkType) -> Tool {
        switch ink {
        case .pencil, .crayon: .pencil
        case .marker, .watercolor: .marker
        case .monoline: .monoline
        // PencilKit's fountain pen is the one ink that answers to barrel
        // roll; the core renders it as a flat nib.
        case .fountainPen: .fountain
        default: .pen
        }
    }

    static func selection(inking: PKInkingTool) -> BrushSelection {
        let type = inking.inkType
        let width = Float(inking.width) * (widthScale[type] ?? 1)
        return BrushSelection(
            brush: BrushRef(tool: tool(type), baseWidth: width, custom: custom(type)),
            color: pack(inking.color, alphaScale: opacityScale[type] ?? 1))
    }

    /// A custom picker item (iOS 18): a library brush at the item's colour
    /// and width; `nil` when the library has no brush under its identifier.
    @available(iOS 18.0, *)
    @MainActor
    static func selection(item: PKToolPickerCustomItem) -> BrushSelection? {
        guard let brush = BrushLibrary.shared.brush(id: item.identifier) else { return nil }
        return BrushSelection(
            brush: BrushRef(
                tool: brush.tool, baseWidth: Float(item.width),
                custom: CustomBrush(id: brush.id, spec: brush.spec)),
            color: pack(item.color))
    }

    /// The initial selection under `-tool <name>` (UI tests); pen otherwise.
    static func selection(named name: String, width: Float = 10) -> BrushSelection? {
        let type: PKInkingTool.InkType
        switch name {
        case "pen": type = .pen
        case "pencil": type = .pencil
        case "marker": type = .marker
        case "monoline": type = .monoline
        case "fountain", "fountainPen": type = .fountainPen
        case "crayon": type = .crayon
        default: return nil
        }
        return selection(inking: PKInkingTool(type, color: .black, width: CGFloat(width)))
    }

    /// Pack as 0xRRGGBBAA, alpha scaled by `alphaScale`.
    static func pack(_ color: UIColor, alphaScale: Float = 1) -> UInt32 {
        var r: CGFloat = 0
        var g: CGFloat = 0
        var b: CGFloat = 0
        var a: CGFloat = 0
        color.getRed(&r, green: &g, blue: &b, alpha: &a)
        func byte(_ v: CGFloat) -> UInt32 { UInt32((max(0, min(1, v)) * 255).rounded()) }
        return byte(r) << 24 | byte(g) << 16 | byte(b) << 8 | byte(a * CGFloat(alphaScale))
    }

    static func unpack(_ v: UInt32) -> UIColor {
        UIColor(
            red: CGFloat((v >> 24) & 0xff) / 255,
            green: CGFloat((v >> 16) & 0xff) / 255,
            blue: CGFloat((v >> 8) & 0xff) / 255,
            alpha: CGFloat(v & 0xff) / 255)
    }
}

/// What every element exposes regardless of kind; UniFFI enums carry no
/// methods, so the accessors live here.
extension Element {
    var id: String {
        switch self {
        case .stroke(let s): s.id
        case .shape(let s): s.id
        }
    }

    var color: UInt32 {
        switch self {
        case .stroke(let s): s.color
        case .shape(let s): s.color
        }
    }

    var isShape: Bool {
        if case .shape = self { return true }
        return false
    }
}
