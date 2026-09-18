// Ink pipeline: the desktop twin of ios/Pendant/Sources/Shaders.metal.
// Vertices are the core's `InkMesh.vertices` (canvas position with y
// flipped into bevy's y-up space, stroke-space uv, per-vertex opacity);
// each mesh entity carries a `MeshTag` indexing the `StrokeStyle` array
// that holds everything per stroke: linear colour, edge mask, grain and
// blend/overlap flags. One shader serves every brush; the only pipeline
// splits (blend, depth compare) live in `InkMaterial::specialize`.
// Layouts must match ink_material.rs (`StrokeStyle`, `InkParams`, vertex
// attributes) and Shaders.metal.
//
// Colour: the render target is sRGB-encoded and blending happens in
// linear light; output is premultiplied `(rgb·a, a)`.
//
// Image masks (kind 2) and grains (kind 2) sample the two texture arrays
// built by ink_assets.rs; a style names its layer in mask_layer /
// grain_layer.

#import bevy_sprite::mesh2d_functions as mesh_functions

// 64 bytes; identical layout in ink_material.rs and Shaders.metal.
struct StrokeStyle {
    color: vec4<f32>,       // linear RGBA, straight alpha (opacity folded in)
    mask: vec4<f32>,        // aspect, corner, hardness, 0
    grain: vec4<f32>,       // scale, strength, 0, 0
    depth: f32,             // unused here: z comes from the entity transform
    flags: u32,             // see MASK_* / FLAG_* below
    mask_layer: u32,
    grain_layer: u32,       // image layer for grain kind 2, hash seed for kind 1
};

struct InkParams {
    zoom: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
};

const MASK_KIND: u32 = 3u;   // bits 0-1: 0 none, 1 shape (tip space), 2 image, 3 ribbon edge
const GRAIN_KIND: u32 = 12u; // bits 2-3: 0 none, 1 noise, 2 image
const FLAG_GRAIN_STROKE: u32 = 16u;
// Bits 5 (multiply) and 6 (discard) pick the pipeline on the CPU side;
// the fragment still reads discard for the stippled edge.
const FLAG_DISCARD: u32 = 64u;
// Cell size of the stipple that softens edges under write-once ink.
const EDGE_CELL: f32 = 0.75;
const EDGE_SEED: u32 = 0x9e37u;

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<storage, read> styles: array<StrokeStyle>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> params: InkParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var masks: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var mask_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var grains: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(5) var grain_sampler: sampler;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) opacity: f32,
};

struct InkOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) canvas: vec2<f32>,
    @location(2) opacity: f32,
    @location(3) @interpolate(flat) stroke: u32,
};

@vertex
fn vertex(vertex: Vertex) -> InkOutput {
    var out: InkOutput;
    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
    out.position = mesh_functions::mesh2d_position_local_to_clip(
        world_from_local,
        vec4<f32>(vertex.position, 1.0),
    );
    out.uv = vertex.uv;
    // Back to canvas space (y down) so grain lands on the same texels as
    // on the iPad.
    out.canvas = vec2<f32>(vertex.position.x, -vertex.position.y);
    out.opacity = vertex.opacity;
    out.stroke = mesh_functions::get_tag(vertex.instance_index);
    return out;
}

// pcg2d integer hash: bit-identical on Metal and WGSL.
fn pcg2d(p: vec2<u32>) -> vec2<u32> {
    var v = p * 1664525u + 1013904223u;
    v.x += v.y * 1664525u;
    v.y += v.x * 1664525u;
    v ^= v >> vec2<u32>(16u);
    v.x += v.y * 1664525u;
    v.y += v.x * 1664525u;
    v ^= v >> vec2<u32>(16u);
    return v;
}

fn hash_corner(c: vec2<f32>, s: u32) -> f32 {
    let q = pcg2d(vec2<u32>(vec2<i32>(c)) + vec2<u32>(s, s * 7u + 1u));
    return f32(q.x & 0xffffu) / 65535.0;
}

fn value_noise(p: vec2<f32>, s: u32) -> f32 {
    let i = floor(p);
    var f = p - i;
    f = f * f * (3.0 - 2.0 * f);
    let a = hash_corner(i, s);
    let b = hash_corner(i + vec2<f32>(1.0, 0.0), s);
    let c = hash_corner(i + vec2<f32>(0.0, 1.0), s);
    let d = hash_corner(i + vec2<f32>(1.0, 1.0), s);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

@fragment
fn fragment(in: InkOutput) -> @location(0) vec4<f32> {
    let s = styles[in.stroke];
    // Sampled in uniform control flow (derivatives for mip selection);
    // only the branches below use the results.
    let grain_anchor = select(in.canvas, in.uv, (s.flags & FLAG_GRAIN_STROKE) != 0u);
    let grain_p = grain_anchor / max(s.grain.x, 1e-3);
    let mask_sample = textureSample(masks, mask_sampler, in.uv * 0.5 + 0.5, s.mask_layer).r;
    let grain_sample = textureSample(grains, grain_sampler, grain_p, s.grain_layer).r;
    var m = 1.0;
    switch (s.flags & MASK_KIND) {
        case 1u: {
            // Rounded superellipse in tip space [-1,1]² (stamped dabs).
            let r = s.mask.y;
            let q = abs(in.uv) - vec2<f32>(1.0 - r);
            let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
            let w = max(fwidth(d), 1.0 - s.mask.z);
            if ((s.flags & FLAG_DISCARD) != 0u) {
                // Write-once ink: stipple the soft band instead of
                // feathering it (see the ribbon case).
                let t = (d + w) / w;
                if (d > 0.0 || (t > 0.0 && hash_corner(floor(in.canvas / EDGE_CELL), EDGE_SEED) < t)) {
                    discard;
                }
            } else {
                m = 1.0 - smoothstep(-w, 0.0, d);
            }
        }
        case 2u: {
            // Greyscale image over the dab, white is ink. Write-once ink
            // cannot carry the partial coverage, so it keeps a fragment
            // with probability equal to the coverage instead.
            if (any(abs(in.uv) > vec2<f32>(1.0))) {
                discard;
            }
            if ((s.flags & FLAG_DISCARD) != 0u) {
                if (hash_corner(floor(in.canvas / EDGE_CELL), EDGE_SEED) > mask_sample) {
                    discard;
                }
            } else {
                m = mask_sample;
            }
        }
        case 3u: {
            // Ribbon: v is the side in -1..1; soften the outer band that
            // hardness leaves. MSAA covers the hard edge itself.
            let h = s.mask.z;
            if (h < 1.0) {
                let t = (abs(in.uv.y) - h) / (1.0 - h);
                if ((s.flags & FLAG_DISCARD) != 0u) {
                    // Write-once ink keeps the first fragment at a pixel,
                    // so the band cannot carry partial alpha: thin it by
                    // dropping fragments where the paper noise says so.
                    if (t > 0.0 && hash_corner(floor(in.canvas / EDGE_CELL), EDGE_SEED) < t) {
                        discard;
                    }
                } else {
                    m = 1.0 - smoothstep(0.0, 1.0, t);
                }
            }
        }
        default: {}
    }
    var g = 1.0;
    let grain_kind = s.flags & GRAIN_KIND;
    if (grain_kind == 4u) {
        let n = value_noise(grain_p, s.grain_layer);
        g = mix(1.0, n, s.grain.y * saturate(s.grain.x * params.zoom / 1.5));
    } else if (grain_kind == 8u) {
        // Mipmapped, so no fade is needed as the view zooms out.
        g = mix(1.0, grain_sample, s.grain.y);
    }
    let a = s.color.a * in.opacity * m * g;
    if (a < 1.0 / 255.0) {
        discard;
    }
    return vec4<f32>(s.color.rgb * a, a);
}
