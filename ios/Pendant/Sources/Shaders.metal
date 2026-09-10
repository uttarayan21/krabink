// Ink pipeline: flat-coloured triangles from the core's stroke meshes.
// Positions arrive in canvas units (x right, y down); `Uniforms` carries
// the canvas → clip-space affine map for the current pan/zoom and the
// stroke colour. Layout must match `Uniforms` in InkRenderer.swift.

#include <metal_stdlib>
using namespace metal;

struct Uniforms {
    float2 scale;
    float2 translate;
    float4 color;
};

struct InkVertex {
    float4 position [[position]];
    float4 color;
};

vertex InkVertex ink_vertex(
    const device float2 *positions [[buffer(0)]],
    constant Uniforms &u [[buffer(1)]],
    uint vid [[vertex_id]])
{
    InkVertex out;
    float2 p = positions[vid] * u.scale + u.translate;
    out.position = float4(p, 0.0, 1.0);
    out.color = u.color;
    return out;
}

fragment float4 ink_fragment(InkVertex in [[stage_in]])
{
    return in.color;
}
