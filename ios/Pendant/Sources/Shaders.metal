// Ink pipeline: flat-coloured triangles from the core's stroke meshes.
// Vertices carry canvas-space position (x right, y down) and their stroke's
// colour, so every committed stroke can live in one batched buffer and be
// drawn with a single call. `Uniforms` is the canvas → clip-space affine
// map for the current pan/zoom. Layouts must match InkRenderer.swift
// (`InkVertex`, `Uniforms`).

#include <metal_stdlib>
using namespace metal;

struct VertexIn {
    float2 position [[attribute(0)]];
    float4 color [[attribute(1)]];
};

struct Uniforms {
    float2 scale;
    float2 translate;
};

struct InkVertex {
    float4 position [[position]];
    float4 color;
};

vertex InkVertex ink_vertex(VertexIn in [[stage_in]], constant Uniforms &u [[buffer(1)]])
{
    InkVertex out;
    float2 p = in.position * u.scale + u.translate;
    out.position = float4(p, 0.0, 1.0);
    out.color = in.color;
    return out;
}

fragment float4 ink_fragment(InkVertex in [[stage_in]])
{
    return in.color;
}
