// PKStroke <-> CRDT Stroke conversion. Control points cross the boundary
// as-is (kind = bsplineControl) including per-point size — the iM2 hardware
// spike showed PencilKit derives size from more than force, so collapsing
// it to a stroke-level width visibly changes real Pencil strokes.

import PencilKit
import PendantCore
import UIKit

enum StrokeCodec {
    static func inkType(_ tool: Tool) -> PKInkingTool.InkType {
        switch tool {
        case .pen: .pen
        case .marker: .marker
        case .monoline: .monoline
        }
    }

    static func tool(_ ink: PKInkingTool.InkType) -> Tool {
        switch ink {
        case .marker: .marker
        case .monoline: .monoline
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

    static func encode(_ stroke: PKStroke, id: String) -> Stroke {
        let baseWidth = Float(stroke.path.first?.size.width ?? 10)
        let points = stroke.path.map { p in
            StrokePoint(
                x: Float(p.location.x),
                y: Float(p.location.y),
                force: Float(p.force),
                tMs: UInt32(max(0, p.timeOffset * 1000)),
                tilt: Tilt(azimuth: Float(p.azimuth), altitude: Float(p.altitude)),
                size: PointSize(w: Float(p.size.width), h: Float(p.size.height)))
        }
        return Stroke(
            id: id,
            tool: tool(stroke.ink.inkType),
            color: pack(stroke.ink.color),
            baseWidth: baseWidth,
            kind: .bsplineControl,
            points: points,
            createdMs: UInt64(max(0, stroke.path.creationDate.timeIntervalSince1970 * 1000)))
    }

    static func decode(_ stroke: Stroke) -> PKStroke {
        let width = CGFloat(stroke.baseWidth)
        let color = unpack(stroke.color)
        let points = stroke.points.map { p in
            PKStrokePoint(
                location: CGPoint(x: CGFloat(p.x), y: CGFloat(p.y)),
                timeOffset: TimeInterval(p.tMs) / 1000,
                size: p.size.map { CGSize(width: CGFloat($0.w), height: CGFloat($0.h)) }
                    ?? CGSize(width: width, height: width),
                opacity: 1,
                force: CGFloat(p.force),
                azimuth: CGFloat(p.tilt?.azimuth ?? 0),
                altitude: CGFloat(p.tilt?.altitude ?? 0))
        }
        let path = PKStrokePath(
            controlPoints: points,
            creationDate: Date(timeIntervalSince1970: TimeInterval(stroke.createdMs) / 1000))
        return PKStroke(ink: PKInk(inkType(stroke.tool), color: color), path: path)
    }
}
