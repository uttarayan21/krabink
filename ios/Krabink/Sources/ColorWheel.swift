// Colour wheel for the paper picker in Settings: hue by angle (red at
// three o'clock, running clockwise), saturation growing from the centre
// out, and a brightness bar beside it. The desktop's wheel
// (crates/krabink/src/color_wheel.rs) uses the same mapping and the same
// HSV maths, so a colour sits in the same spot on both.

import SwiftUI

/// Hue, saturation and value in `0...1` over sRGB components. Kept as the
/// picker's state rather than derived from the colour, so the hue
/// survives dragging to grey or black.
struct HSV: Equatable {
    var h: Double
    var s: Double
    var v: Double

    init(h: Double, s: Double, v: Double) {
        self.h = h
        self.s = s
        self.v = v
    }

    init(hex: UInt32) {
        let r = Double((hex >> 16) & 0xFF) / 255
        let g = Double((hex >> 8) & 0xFF) / 255
        let b = Double(hex & 0xFF) / 255
        let maxC = max(r, g, b)
        let delta = maxC - min(r, g, b)
        let sector: Double
        if delta == 0 {
            sector = 0
        } else if maxC == r {
            let raw = ((g - b) / delta).truncatingRemainder(dividingBy: 6)
            sector = raw < 0 ? raw + 6 : raw
        } else if maxC == g {
            sector = (b - r) / delta + 2
        } else {
            sector = (r - g) / delta + 4
        }
        self.init(h: sector / 6, s: maxC == 0 ? 0 : delta / maxC, v: maxC)
    }

    var hex: UInt32 {
        let wrapped = h - h.rounded(.down)
        let sector = wrapped * 6
        let chroma = v * s
        let x = chroma * (1 - abs(sector.truncatingRemainder(dividingBy: 2) - 1))
        let rgb: (r: Double, g: Double, b: Double) =
            switch Int(sector) {
            case 0: (chroma, x, 0)
            case 1: (x, chroma, 0)
            case 2: (0, chroma, x)
            case 3: (0, x, chroma)
            case 4: (x, 0, chroma)
            default: (chroma, 0, x)
            }
        let m = v - chroma
        func byte(_ c: Double) -> UInt32 { UInt32(min(max(((c + m) * 255).rounded(), 0), 255)) }
        return byte(rgb.r) << 16 | byte(rgb.g) << 8 | byte(rgb.b)
    }

    var color: Color { Color(hex: hex) }
}

/// Hue by angle, saturation by distance from the centre, shaded to
/// `hsv.v`. Pressing or dragging anywhere on it picks.
struct ColorWheel: View {
    @Binding var hsv: HSV

    /// Red, yellow, green, cyan, blue, magenta, red: HSV is linear in RGB
    /// between these, so the gradient is exact.
    private static let hues = (0...6).map {
        Color(hue: Double($0) / 6, saturation: 1, brightness: 1)
    }

    var body: some View {
        GeometryReader { geo in
            let side = min(geo.size.width, geo.size.height)
            let radius = side / 2
            let center = CGPoint(x: radius, y: radius)
            let angle = hsv.h * 2 * Double.pi
            let reach = radius * CGFloat(hsv.s)
            ZStack {
                Circle()
                    .fill(AngularGradient(
                        colors: Self.hues, center: .center,
                        startAngle: .zero, endAngle: .degrees(360)))
                // White towards the centre: exactly HSV's saturation at
                // full value; the black veil below is exactly its value.
                Circle()
                    .fill(RadialGradient(
                        colors: [.white, .white.opacity(0)], center: .center,
                        startRadius: 0, endRadius: radius))
                Circle()
                    .fill(Color.black.opacity(1 - hsv.v))
                Circle()
                    .stroke(Theme.border, lineWidth: 1)
                PickMarker(fill: hsv.color)
                    .position(
                        x: center.x + reach * CGFloat(cos(angle)),
                        y: center.y + reach * CGFloat(sin(angle)))
            }
            .frame(width: side, height: side)
            .contentShape(Circle())
            .gesture(
                DragGesture(minimumDistance: 0).onChanged { drag in
                    let dx = Double(drag.location.x - center.x)
                    let dy = Double(drag.location.y - center.y)
                    let turn = atan2(dy, dx) / (2 * Double.pi)
                    hsv.h = turn < 0 ? turn + 1 : turn
                    hsv.s = min(hypot(dx, dy) / Double(radius), 1)
                })
        }
        .aspectRatio(1, contentMode: .fit)
        .accessibilityElement()
        .accessibilityLabel("paper hue")
        .accessibilityValue("hue \(Int(hsv.h * 360)) degrees, saturation \(Int(hsv.s * 100)) percent")
        .accessibilityAdjustableAction { direction in
            let step = direction == .increment ? 1.0 / 36 : -1.0 / 36
            hsv.h = (hsv.h + step + 1).truncatingRemainder(dividingBy: 1)
        }
    }
}

/// Vertical brightness bar: full value at the top, black at the bottom,
/// in the wheel's current hue and saturation.
struct BrightnessBar: View {
    @Binding var hsv: HSV

    var body: some View {
        GeometryReader { geo in
            let height = geo.size.height
            ZStack(alignment: .top) {
                RoundedRectangle(cornerRadius: 4, style: .continuous)
                    .fill(LinearGradient(
                        colors: [HSV(h: hsv.h, s: hsv.s, v: 1).color, .black],
                        startPoint: .top, endPoint: .bottom))
                    .overlay {
                        RoundedRectangle(cornerRadius: 4, style: .continuous)
                            .stroke(Theme.border, lineWidth: 1)
                    }
                PickMarker(fill: hsv.color)
                    .position(x: geo.size.width / 2, y: CGFloat(1 - hsv.v) * height)
            }
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0).onChanged { drag in
                    hsv.v = 1 - min(max(Double(drag.location.y / height), 0), 1)
                })
        }
        .accessibilityElement()
        .accessibilityLabel("paper brightness")
        .accessibilityValue("\(Int(hsv.v * 100)) percent")
        .accessibilityAdjustableAction { direction in
            let step = direction == .increment ? 0.05 : -0.05
            hsv.v = min(max(hsv.v + step, 0), 1)
        }
    }
}

/// Picked-colour dot with a white-and-black ring, visible on any colour.
private struct PickMarker: View {
    let fill: Color

    var body: some View {
        Circle()
            .fill(fill)
            .frame(width: 14, height: 14)
            .overlay { Circle().stroke(.white, lineWidth: 2) }
            .overlay { Circle().stroke(.black.opacity(0.6), lineWidth: 1).padding(-2) }
            .allowsHitTesting(false)
    }
}
