#!/usr/bin/env python3
"""Render the App Store icon (1024x1024, opaque RGB PNG) for the iPad app.

Mirrors `LogoMark` in ios/Pendant/Sources/Theme.swift: the indigo accent
gradient with a white pencil tip. Pure stdlib (zlib + struct) so it runs
anywhere; the output is committed at
ios/Pendant/Assets.xcassets/AppIcon.appiconset/AppIcon.png and only needs
regenerating when the mark changes.

    python3 scripts/gen-app-icon.py [out.png] [--size 1024]

iOS masks the corners itself and App Store Connect rejects icons with an
alpha channel, so the gradient runs edge to edge with no transparency.
"""

import math
import struct
import sys
import zlib
from pathlib import Path

ACCENT = (0x7C, 0x8C, 0xFF)  # Theme.accent
BG = (0x11, 0x11, 0x17)  # Theme.bg
WHITE = (0xFF, 0xFF, 0xFF)


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


# LogoMark: accent at the top-left fading to accent at 60% opacity over the
# dark page at the bottom-right.
GRAD_END = mix(ACCENT, BG, 0.4)


def half_plane_sdf(polygon):
    """Signed distance to a convex polygon given as CCW vertices (y down)."""
    edges = []
    n = len(polygon)
    for i in range(n):
        (x0, y0), (x1, y1) = polygon[i], polygon[(i + 1) % n]
        ex, ey = x1 - x0, y1 - y0
        length = math.hypot(ex, ey)
        # Outward normal for a CCW polygon in a y-down frame.
        nx, ny = ey / length, -ex / length
        edges.append((x0, y0, nx, ny))

    def sdf(px, py):
        return max((px - x0) * nx + (py - y0) * ny for x0, y0, nx, ny in edges)

    return sdf


def pencil(size):
    """Silhouette and lead polygons of a pencil pointing to the bottom-left.

    Body and tip are one convex pentagon: a union of two polygons would leave
    a half-covered seam along their shared edge.
    """
    c = size / 2
    s = size / 1024
    ux, uy = 1 / math.sqrt(2), -1 / math.sqrt(2)  # along the pencil, up-right
    vx, vy = 1 / math.sqrt(2), 1 / math.sqrt(2)  # across
    length, width, tip = 560 * s, 150 * s, 170 * s
    lead = 0.42

    def at(a, b):
        return (c + ux * a + vx * b, c + uy * a + vy * b)

    half = width / 2
    back, front = length / 2, -length / 2
    shoulder = front + tip
    point = at(front, 0)
    outline = [point, at(shoulder, -half), at(back, -half), at(back, half), at(shoulder, half)]
    lead_poly = [point, at(front + tip * lead, -half * lead), at(front + tip * lead, half * lead)]
    # Ensure CCW in the y-down frame (signed area > 0).
    return [ccw(p) for p in (outline, lead_poly)]


def ccw(poly):
    area = 0
    for i in range(len(poly)):
        (x0, y0), (x1, y1) = poly[i], poly[(i + 1) % len(poly)]
        area += x0 * y1 - x1 * y0
    return poly if area > 0 else list(reversed(poly))


def coverage(d):
    # One-pixel anti-aliased edge.
    return min(1.0, max(0.0, 0.5 - d))


def render(size):
    outline, lead = pencil(size)
    outline_sdf, lead_sdf = (half_plane_sdf(p) for p in (outline, lead))
    xs = [x for x, _ in outline]
    ys = [y for _, y in outline]
    x_lo, x_hi = int(min(xs)) - 2, int(max(xs)) + 3
    y_lo, y_hi = int(min(ys)) - 2, int(max(ys)) + 3
    # A subtle drop shadow under the pencil, offset like LogoMark's.
    shadow_dx, shadow_dy, shadow_blur = 0, 10 * size / 1024, 22 * size / 1024
    shadow_color = mix(BG, ACCENT, 0.15)

    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            t = (x + y) / (2 * (size - 1))
            r, g, b = mix(ACCENT, GRAD_END, t)
            if x_lo <= x <= x_hi + shadow_blur and y_lo <= y <= y_hi + shadow_blur:
                px, py = x + 0.5, y + 0.5
                sx, sy = px - shadow_dx, py - shadow_dy
                shadow_d = outline_sdf(sx, sy)
                shadow = 0.35 * min(1.0, max(0.0, 1 - shadow_d / shadow_blur))
                r, g, b = mix((r, g, b), shadow_color, shadow)
                white = coverage(outline_sdf(px, py))
                r, g, b = mix((r, g, b), WHITE, white)
                dark = coverage(lead_sdf(px, py))
                r, g, b = mix((r, g, b), BG, dark)
            row += bytes((round(r), round(g), round(b)))
        rows.append(b"\x00" + bytes(row))
    return b"".join(rows)


def png(size, raw):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)  # 8-bit RGB
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main(argv):
    size = 1024
    out = None
    args = iter(argv[1:])
    for arg in args:
        if arg == "--size":
            size = int(next(args))
        else:
            out = Path(arg)
    if out is None:
        out = (
            Path(__file__).resolve().parent.parent
            / "ios/Pendant/Assets.xcassets/AppIcon.appiconset/AppIcon.png"
        )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(png(size, render(size)))
    print(f"wrote {out} ({size}x{size})")


if __name__ == "__main__":
    main(sys.argv)
