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
        // PencilKit's fountain pen is the one ink that answers to barrel
        // roll; the core renders it as a flat nib.
        case .brush: .fountainPen
        }
    }

    static func tool(_ ink: PKInkingTool.InkType) -> Tool {
        switch ink {
        case .marker: .marker
        case .monoline: .monoline
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

    /// `samples` are the raw pen samples the observer saw for this stroke;
    /// PencilKit's control points carry azimuth/altitude but not barrel
    /// roll, so roll is looked up from the nearest sample by location.
    static func encode(_ stroke: PKStroke, id: String, samples: [PenSample] = []) -> Stroke {
        let scale = Float(renderedWidthScale(stroke))
        let baseWidth = Float(stroke.path.first?.size.width ?? 10) * scale
        let points = stroke.path.map { p in
            StrokePoint(
                x: Float(p.location.x),
                y: Float(p.location.y),
                force: Float(p.force),
                tMs: UInt32(max(0, p.timeOffset * 1000)),
                tilt: Tilt(
                    azimuth: Float(p.azimuth), altitude: Float(p.altitude),
                    roll: Float(nearestRoll(to: p.location, in: samples))),
                size: PointSize(w: Float(p.size.width) * scale, h: Float(p.size.height) * scale))
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

    /// PencilKit draws some inks (the marker's chisel nib above all) wider
    /// than the point sizes it stores. Its own render bounds tell how wide
    /// the ink really is; scale the stored sizes so every renderer of the
    /// shared model draws what PencilKit drew.
    private static func renderedWidthScale(_ stroke: PKStroke) -> CGFloat {
        let widths = stroke.path.map { $0.size.width }
        guard let maxWidth = widths.max(), maxWidth > 0, !stroke.path.isEmpty else { return 1 }
        var box = CGRect.null
        for p in stroke.path { box = box.union(CGRect(origin: p.location, size: .zero)) }
        let rendered = stroke.renderBounds
        // Each side of the render bounds extends ~half the rendered width
        // beyond the path on the axis where the ink is widest.
        let extra = max(rendered.width - box.width, rendered.height - box.height)
        guard extra > 0 else { return 1 }
        return min(4, max(0.5, extra / maxWidth))
    }

    private static func nearestRoll(to location: CGPoint, in samples: [PenSample]) -> CGFloat {
        var best: (d2: CGFloat, roll: CGFloat) = (.greatestFiniteMagnitude, 0)
        for s in samples {
            let dx = s.location.x - location.x
            let dy = s.location.y - location.y
            let d2 = dx * dx + dy * dy
            if d2 < best.d2 { best = (d2, s.roll) }
        }
        return best.roll
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
