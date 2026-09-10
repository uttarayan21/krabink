// PencilKit tool picker ↔ core stroke model. Only the picker survives from
// PencilKit: its ink type maps onto the core `Tool`, its colour onto the
// packed RGBA the CRDT stores. Stroke geometry never touches PencilKit.

import PencilKit
import PendantCore
import UIKit

enum StrokeCodec {
    static func tool(_ ink: PKInkingTool.InkType) -> Tool {
        switch ink {
        case .marker: .marker
        case .monoline: .monoline
        // PencilKit's fountain pen is the one ink that answers to barrel
        // roll; the core renders it as a flat nib.
        case .fountainPen: .brush
        default: .pen
        }
    }

    /// Pack as 0xRRGGBBAA.
    static func pack(_ color: UIColor) -> UInt32 {
        var r: CGFloat = 0
        var g: CGFloat = 0
        var b: CGFloat = 0
        var a: CGFloat = 0
        color.getRed(&r, green: &g, blue: &b, alpha: &a)
        func byte(_ v: CGFloat) -> UInt32 { UInt32((max(0, min(1, v)) * 255).rounded()) }
        return byte(r) << 24 | byte(g) << 16 | byte(b) << 8 | byte(a)
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
