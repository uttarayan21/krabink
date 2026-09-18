// Ink pipeline. Vertices are the core's `InkMesh.vertices` uploaded
// verbatim (canvas position, stroke-space uv, per-vertex opacity) plus a
// per-vertex stroke index into a `StrokeStyle` array that carries
// everything per stroke: linear colour, edge mask, grain, depth slot and
// blend/overlap flags. One fragment shader serves every brush; the only
// pipeline splits are blend (Normal/Multiply) and the depth state that
// implements write-once ink (Overlap::Discard). Layouts must match
// InkRenderer.swift (`Uniforms`, `StrokeStyle`, vertex descriptor) and the
// desktop's ink.wgsl.
//
// Colour: the framebuffer is sRGB-encoded and blending happens in linear
// light; output is premultiplied `(rgb·a, a)`.

#include <metal_stdlib>
using namespace metal;

struct VertexIn {
    float2 position [[attribute(0)]];
    float2 uv       [[attribute(1)]];
    float  opacity  [[attribute(2)]];
    uint   stroke   [[attribute(3)]];
};

struct Uniforms {
    float2 scale;
    float2 translate;
    float  zoom;
    float  pad;
};

// 64 bytes; identical layout in InkRenderer.swift and ink.wgsl.
struct StrokeStyle {
    float4 color;      // linear RGBA, straight alpha (opacity folded in)
    float4 mask;       // aspect, corner, hardness, 0
    float4 grain;      // scale, strength, 0, 0
    float  depth;      // NDC z slot, strictly monotone in draw order
    uint   flags;      // see MASK_* / FLAG_* below
    uint   maskLayer;
    uint   grainLayer; // image layer for grain kind 2, hash seed for kind 1
};

constant uint MASK_KIND   = 3u;   // bits 0-1: 0 none, 1 shape (tip space), 2 image, 3 ribbon edge
constant uint GRAIN_KIND  = 12u;  // bits 2-3: 0 none, 1 noise, 2 image
constant uint FLAG_GRAIN_STROKE = 16u;
constant uint FLAG_MULTIPLY     = 32u;
constant uint FLAG_DISCARD      = 64u;
// Cell size of the stipple that softens edges under write-once ink.
constant float EDGE_CELL = 0.75;
constant uint  EDGE_SEED = 0x9e37u;

struct V2F {
    float4 position [[position]];
    float2 uv;
    float2 canvas;
    float  opacity;
    uint   stroke [[flat]];
};

vertex V2F ink_vertex(VertexIn in [[stage_in]],
                      constant Uniforms &u [[buffer(1)]],
                      constant StrokeStyle *styles [[buffer(2)]])
{
    V2F out;
    float2 p = in.position * u.scale + u.translate;
    out.position = float4(p, styles[in.stroke].depth, 1.0);
    out.uv = in.uv;
    out.canvas = in.position;
    out.opacity = in.opacity;
    out.stroke = in.stroke;
    return out;
}

// pcg2d integer hash: bit-identical on Metal and WGSL.
static uint2 pcg2d(uint2 v)
{
    v = v * 1664525u + 1013904223u;
    v.x += v.y * 1664525u;
    v.y += v.x * 1664525u;
    v ^= v >> 16u;
    v.x += v.y * 1664525u;
    v.y += v.x * 1664525u;
    v ^= v >> 16u;
    return v;
}

static float lattice(float2 c, uint s)
{
    uint2 q = pcg2d(uint2(int2(c)) + uint2(s, s * 7u + 1u));
    return float(q.x & 0xffffu) / 65535.0;
}

static float value_noise(float2 p, uint s)
{
    float2 i = floor(p);
    float2 f = p - i;
    f = f * f * (3.0 - 2.0 * f);
    float a = lattice(i, s);
    float b = lattice(i + float2(1, 0), s);
    float c = lattice(i + float2(0, 1), s);
    float d = lattice(i + float2(1, 1), s);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

fragment float4 ink_fragment(V2F in [[stage_in]],
                             constant StrokeStyle *styles [[buffer(0)]],
                             constant Uniforms &u [[buffer(1)]],
                             texture2d_array<float> masks [[texture(0)]],
                             texture2d_array<float> grains [[texture(1)]],
                             sampler clampMip [[sampler(0)]],
                             sampler repeatMip [[sampler(1)]])
{
    StrokeStyle s = styles[in.stroke];
    float2 grainAnchor = (s.flags & FLAG_GRAIN_STROKE) ? in.uv : in.canvas;
    float2 grainP = grainAnchor / max(s.grain.x, 1e-3);
    float m = 1.0;
    switch (s.flags & MASK_KIND) {
        case 1u: {
            // Rounded superellipse in tip space [-1,1]² (stamped dabs).
            float r = s.mask.y;
            float2 q = abs(in.uv) - (1.0 - r);
            float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
            float w = max(fwidth(d), 1.0 - s.mask.z);
            if (s.flags & FLAG_DISCARD) {
                // Write-once ink: stipple the soft band instead of
                // feathering it (see the ribbon case).
                float t = (d + w) / w;
                if (d > 0.0 || (t > 0.0 && lattice(floor(in.canvas / EDGE_CELL), EDGE_SEED) < t))
                    discard_fragment();
            } else {
                m = 1.0 - smoothstep(-w, 0.0, d);
            }
            break;
        }
        case 2u: {
            // Greyscale image over the dab, white is ink. Write-once ink
            // cannot carry partial coverage, so it keeps a fragment with
            // probability equal to the coverage instead.
            if (any(abs(in.uv) > 1.0)) discard_fragment();
            float cover = masks.sample(clampMip, in.uv * 0.5 + 0.5, s.maskLayer).r;
            if (s.flags & FLAG_DISCARD) {
                if (lattice(floor(in.canvas / EDGE_CELL), EDGE_SEED) > cover) discard_fragment();
            } else {
                m = cover;
            }
            break;
        }
        case 3u: {
            // Ribbon: v is the side in -1..1; soften the outer band that
            // hardness leaves. MSAA covers the hard edge itself.
            float h = s.mask.z;
            if (h < 1.0) {
                float t = (abs(in.uv.y) - h) / (1.0 - h);
                if (s.flags & FLAG_DISCARD) {
                    // Write-once ink keeps the first fragment at a pixel,
                    // so the band cannot carry partial alpha: thin it by
                    // dropping fragments where the paper noise says so.
                    if (t > 0.0 && lattice(floor(in.canvas / EDGE_CELL), EDGE_SEED) < t)
                        discard_fragment();
                } else {
                    m = 1.0 - smoothstep(0.0, 1.0, t);
                }
            }
            break;
        }
        default: break;
    }
    float g = 1.0;
    uint grainKind = s.flags & GRAIN_KIND;
    if (grainKind == 4u) {
        float n = value_noise(grainP, s.grainLayer);
        g = mix(1.0, n, s.grain.y * saturate(s.grain.x * u.zoom / 1.5));
    } else if (grainKind == 8u) {
        // Mipmapped, so no fade is needed as the view zooms out.
        g = mix(1.0, grains.sample(repeatMip, grainP, s.grainLayer).r, s.grain.y);
    }
    float a = s.color.a * in.opacity * m * g;
    if (a < 1.0 / 255.0) discard_fragment();
    return float4(s.color.rgb * a, a);
}
