# Brush engine: stamp/texture brushes for Krabink

Status: P0–P4 implemented (notes at the end). P4's decision: the EMA model ships, ISM stays a feature-gated trial.

## Context

Ink today is "tool → width → lyon round-join mesh → flat colour". Only the pen varies width (pressure + speed); marker, monoline and fountain pen are constant width; the fountain nib has no caps; the marker double-darkens at every join and self-crossing because every triangle blends independently; there is no pencil, no texture, no opacity per point, and `Tilt.altitude` / `PointSize.h` are stored but never used. The user wants brushes on the level of Concepts / Notability / Apple Notes: pressure pen, tilt-shaded grainy pencil, chisel highlighter that never darkens where it crosses itself, calligraphy nib that follows barrel roll, and custom stamp + grain brushes. Decisions taken with the user (2026-09-10):

- **Direction:** full stamp/texture brush engine (Concepts/Procreate/Google-Ink style), not just vector polish.
- **Input modeling:** trial `ink-stroke-modeler-rs` side by side with the existing EMA modeler, keep the winner.
- **Tools that matter:** pen, pencil (tilt + grain), marker/highlighter (chisel, translucent), fountain/calligraphy (nib + roll). All four.
- **Desktop parity:** lockstep. Every phase ships the Metal (iOS) and WGSL (bevy desktop) renderer together; core is the single source of truth.

Guiding rule: **every renderer draws from the same core output**. The live tail, the wet ink another device receives, the committed stroke, the thumbnail and the SVG export all run the same pure fold in `krabink-core`.

## Research digest (what other apps do)

| App / lib | Model | Takeaways used here |
|---|---|---|
| **Google Ink** (androidx.ink, Apache-2, C++) | `BrushFamily = InputModel + coats[{BrushTip, BrushPaint}]`. Tip: scale(w,h), corner_rounding, slant, pinch, rotation, particle gap (0 = continuous, else stamps). Behaviors: source (pressure, tilt, orientation, speed, direction, distance, time, predicted distance, random…) → curve/damping → target (width/height/size mult, rotation/slant/pinch offset, position offset, hue/chroma/lightness, opacity mult). Paint: texture layers (tiling vs stamping, brush-size vs stroke-coord units, per-layer blend), `self_overlap: Any|Accumulate|Discard`. Mesh attrs: position, opacity_shift, color_shift, side/forward derivatives, surface_uv. Tip size rate-of-change limited per distance. | The whole shape of our `BrushSpec` (flat slot list instead of node graph). Tip = rounded superellipse with aspect. Overlap Accumulate/Discard as a per-brush contract. Rate limiting of size changes. Attributed mesh with uv + opacity. |
| **Ciallo** (SIGGRAPH 2024) | GPU vector brush strokes: one quad per polyline edge, fragment SDF; stamps integrated analytically in the fragment using an arc-length prefix sum; airbrush closed form. Real-time editing. | Kept as a future optimisation behind the same `InkMesh` type. Our stamps are explicit dab quads with an SDF mask in the fragment (zoom independent), simpler on two GPU backends. |
| **Procreate** | Brush = shape (stamp) + grain (texture); grain "texturized" (locked to canvas) vs "moving" (dragged with stroke); spacing, scatter, jitter; stamp rotates with azimuth/roll. | `Grain.mapping: Canvas | Stroke`; `Emit::Stamped {spacing, scatter, rotation_jitter, size_jitter, opacity_jitter}`. |
| **Concepts** | Vector-editable strokes (brush/width changeable after the fact). Stamp brushes (stamps stack, overlap accumulates) vs "reveal" brushes (stamp unmasks a grain → even stroke, no darkening when doubling back). Dynamics on pressure/tilt/velocity for size and opacity. Custom stamp + grain images 256–1024 px, greyscale, seamless grain. | Stored strokes keep raw inputs + spec so re-render with another brush is possible. "Reveal" = our `Overlap::Discard` + canvas-locked grain. Asset constraints for custom brushes. |
| **PencilKit** (Apple Notes/Freeform) | `PKStrokePoint`: location, timeOffset, size (w,h), opacity, force, azimuth, altitude, secondaryScale. Inks: pen, pencil, marker, monoline, fountainPen, watercolor, crayon. Pencil: tilt widens + lightens; marker: chisel, roll controls angle; fountain pen: roll controls nib. Pencil Pro: hover shows tool footprint, squeeze shows picker, `rollAngle` (iOS 17.5), custom `PKToolPickerCustomItem` (iOS 18). Estimated properties (force/azimuth) arrive later via `touchesEstimatedPropertiesUpdated`. | Per-point opacity and (w,h) are outputs of dynamics, not stored. Roll/altitude captured; estimated-property patching added. Picker keeps Apple's UI + custom items. |
| **Notability** | Vector pencil: pressure → opacity, tilt → thickness. Calligraphy pen: nib angle follows roll; presets 0°/30°/45°. | Pencil preset behaviours. Fountain nib fallback angle when no tilt. |
| **perfect-freehand** | `radius = size·ease(0.5 − thinning·(0.5 − pressure))`; pressure simulated from velocity; streamline `t = 0.15 + (1−s)·0.85`; taper by arc length; sharp corners (dot < 0) get a cap fan. | Already mirrored in our EMA modeler; taper moves into dynamics as `DistanceToEnd`. |
| **ink-stroke-modeler** (Google, Rust port used by Rnote) | Spring-mass position model (`spring_mass_constant`, `drag_constant`), loop-contraction mitigation, `min_output_rate` upsampling, end-of-stroke iterations, built-in prediction, stylus state modeled alongside. Unit-agnostic, must be tuned. | P4 trial behind an `InputModel` trait; judged on recorded device strokes (jitter, lag, overshoot). |
| **libmypaint** | Dabs at spacing = fraction of radius; per-dab opacity/hardness; input→setting curves; velocity through first-order lowpass `exp(−dt/T)`. | Speed damping constant per behaviour. |
| **Rnote** (Rust) | Styles Smooth / Rough / Textured. Textured: seed, density, dot distribution (uniform/normal/exponential), pressure curve; seeded PCG advanced per segment → deterministic re-render. | Seeded PCG32 in core, per-stroke seed from the ULID, per-dab stream index so incremental == batch. |
| Single-coverage translucency (general GPU) | Stencil per stroke; **depth trick** (per-vertex z = stroke order, depth test `less` + write: same-stroke fragments at equal z are rejected after the first write, later strokes at closer z still blend over earlier ones, works inside one batched draw and with MSAA); per-stroke offscreen layer; max-blend mask. | Depth trick chosen for `Overlap::Discard` (see Rendering). |

Sources: Google Ink headers (`ink/brush/brush_tip.h`, `brush_behavior.h`, `brush_paint.h`, `geometry/mesh_format.h`), Ciallo SIGGRAPH 2024 abstract, Procreate handbook (Brush Studio), Concepts tutorials (custom brush, stamp & grain), Apple WWDC24 "Squeeze the most out of Apple Pencil", PencilKit `PKStrokePoint`, Notability pencil + calligraphy articles, perfect-freehand sources, google/ink-stroke-modeler `params.h`, MyPaint brush-engine doc, Rnote `rnote-compose/src/style/*`.

## Current pipeline (verified, for reference)

- `crates/krabink-core/src/brush.rs`: `RawSample{x,y,force,t_ms,tilt}`, `BrushParams` per tool (only Pen has thinning 0.5 + end taper), width formula `size·(1+(force−1)·thinning)·(1−thinning·0.5·clamp(speed/speed_ref))·lead_in` floored at `size·min_width`; positional EMA streamline, speed EMA, `min_distance` drop; `State` is `Copy` so `predict()` clones; `finish()` lands on last raw + end taper; `point()` writes `size: Some(PointSize{w,h})` with `h == w`.
- `crates/krabink-core/src/geom.rs`: `stroke_mesh` → lyon `StrokeTessellator` with variable width attribute, round caps/joins, **position-only** vertices; `nib_ribbon` for `Tool::Brush` (θ = azimuth + roll, no caps, altitude unused). `StrokeMesh{positions, indices}`. `DEFAULT_TOLERANCE = 0.25`.
- `crates/krabink-core/src/stroke.rs`: `StrokePoint{x,y,force,t_ms,tilt{azimuth,altitude,roll},size}`; `Tool{Pen,Marker,Monoline,Brush}`; `Stroke{id,tool,color,base_width,kind,points,created_ms}`; postcard `ChunkRepr` (+V1 compat). Loro keys (`note.rs`): `id, elem, tool, color, width, kind, created, points`.
- `wetink.rs`: `WetPoint{x,y,force,width?,nib?}`, `WetInk::Begin{tool,color,base_width}`.
- iOS `InkRenderer.swift`: `InkVertex{float2 pos, uchar4 colour}`, one pipeline, straight-alpha blend, MSAA 4, all committed strokes in one batched draw, wet/live separate buffers, zoom tolerance buckets. `Shaders.metal`: passthrough. `SketchScreen.swift`: force clamped ≤ 1, tilt (azimuth/altitude/roll) for pencil, coalesced + predicted touches, **no** estimated-property updates.
- Desktop `crates/krabink/src/sketch.rs`: bevy `Mesh` position-only + `ColorMaterial` per element, no zoom buckets.
- FFI `IndexedMesh{positions: Vec<f32>, indices}`.

## Architecture

```
raw samples ─► 1. InputModel (Ema | Ism) ─► StrokePoint[] {x,y,force,t_ms,tilt}   (stored in CRDT, on the wet wire)
                                                   │
        BrushSpec (size-relative) + base_width + seed
                                                   ▼
                              2. TipEvaluator ─► TipState[] {x,y,w,h,rot,opacity,arc}
                                                   ▼
                              3. geometry: Continuous (lyon attributed | nib ribbon + rect caps)
                                           or Stamped (one quad per dab, SDF/image mask)
                                                   ▼
                    InkMesh { vertices [x,y,u,v,opacity], indices, style: InkStyle }
                                                   ▼
        iOS Metal / bevy WGSL: alpha = colour.a · opacity · mask(u,v) · grain(pos|uv); depth trick for Discard
```

Stages 2 and 3 consume only `StrokePoint`s, never raw samples. Speed is derived from emitted points' `t_ms`. All randomness is `Pcg32(seed = low 32 bits of the stroke ULID, stream = point/dab index)`, so an incremental live build produces the same dabs as the batch and every device draws the same stroke. Determinism contract: bit-exact within one binary; within 1e-4 canvas units across platforms (libm ulps).

## Core design (`crates/krabink-core`)

### Brush definition

New `src/brush/spec.rs`. Flat struct, fixed behaviour slots. All lengths in multiples of the stroke's `base_width` ("sizes").

```rust
pub struct BrushSpec { pub input: InputParams, pub tip: Tip, pub dynamics: Vec<Behavior>, pub emit: Emit, pub paint: Paint }

pub struct InputParams { pub model: InputModelKind /* Ema | Ism */, pub streamline: f32, pub min_distance: f32, pub min_force: f32 }

pub struct Tip {
    pub aspect: f32,          // h/w: 1 round/square, 0.25 chisel, 0.15 flat nib
    pub corner: f32,          // 0 square … 1 full ellipse (Google Ink corner_rounding)
    pub orient: Orient,       // Motion | Nib { fallback: f32 } | Fixed(f32)
    pub hardness: f32,        // 1 hard edge; lower feathers the mask in the fragment shader
    pub max_size_rate: f32,   // max Δ(w,h) per size travelled
    pub mask: Mask,           // Shape | Image(AssetId)  (Image: P3)
}
pub struct Behavior { pub source: Source, pub curve: Curve /* Linear | Pow(f32) | Smoothstep */, pub range: [f32; 2], pub target: Target /* Width|Height|Size|Opacity|Rotation */, pub damping_ms: f32 }
pub enum Source { Pressure, Speed { max: f32 } /* sizes per second */, Tilt /* 0 upright … 1 flat */, DistanceFromStart { over: f32 }, DistanceToEnd { over: f32 }, Random }
pub enum Emit { Continuous, Stamped { spacing: f32, scatter: f32, rotation_jitter: f32, size_jitter: f32, opacity_jitter: f32 } }
pub struct Paint { pub opacity: f32, pub overlap: Overlap /* Accumulate | Discard */, pub blend: Blend /* Normal | Multiply */, pub grain: Option<Grain> }
pub struct Grain { pub source: GrainSource /* Noise | Paper | Image(AssetId) */, pub mapping: GrainMapping /* Canvas | Stroke */, pub scale: f32, pub strength: f32 }
```

Width/size targets stack multiplicatively, rotation additively (Google Ink rule). Nib width-from-direction is not a behaviour: it falls out of `Orient::Nib` ribbon geometry (`|cross(dir, nib)|·w`, floored, plus nib thickness `aspect·w`).

### Presets (`BrushSpec::preset(tool)`, data only)

| Preset | Tip | Dynamics | Emit | Paint |
|---|---|---|---|---|
| Pen | aspect 1, corner 1, Motion, hardness 1, rate 2.0 | Pressure→Size Linear [0.5, 1.0]; Speed→Size Linear [1.0, 0.75] damping 40 ms; DistanceToEnd{over 1.5}→Size [0.25, 1.0] | Continuous | opacity 1, Accumulate, Normal |
| Monoline | as Pen | none | Continuous | opaque |
| Marker / highlighter | aspect 0.25, corner 0.1, Nib{fallback π/4} | none | Continuous | opacity 0.45, **Discard**, **Multiply** |
| Pencil | aspect 1, corner 1, hardness 0.7 | Tilt→Size [1.0, 2.2]; Tilt→Opacity [1.0, 0.45]; Pressure→Opacity Pow(0.8) [0.3, 1.0]; Pressure→Size [0.8, 1.0] | Continuous (P2); "Pencil (grainy)" Stamped{0.15, 0.1, 0, 0.05, 0.1} (P3) | opacity 0.9, Accumulate, Normal, Grain{Noise, Canvas, scale 1.5, strength 0.55} |
| Fountain (today's `Brush`) | aspect 0.15, corner 0, Nib{fallback −π/4} | Pressure→Width Linear [0.85, 1.15] | Continuous | opaque, Accumulate |

P0 acceptance: Pen preset reproduces today's meshes on the existing corpus within 1e-4 (speed source in sizes/s chosen for parity with today's `speed_ref`). Tuning afterwards is data-only.

### Stroke record and storage

- `Tool` → `Pen | Pencil | Marker | Monoline | Fountain` (`Fountain` serialises as `"brush"`, parses `"brush" | "fountain"`).
- `Stroke` gains `brush: Option<CustomBrush { id: BrushId, spec: BrushSpec }>` (None = preset from `tool`); `Stroke::spec() -> Cow<BrushSpec>`, `Stroke::seed() -> u32` (ULID low bits, documented on `StrokeId`).
- Per-point channels unchanged. `size` stays only as the legacy override (old baked strokes + PencilKit imports); new strokes write `size: None`. **No `ChunkRepr` V2.** Only reason one would appear: `t_ms` resolution stepping visible in speed dynamics (then 0.25 ms).
- Loro keys: existing + `brush` (string id, custom only) + `spec` (postcard `SpecRepr { version: u8, spec }`). Reader: `spec` absent → preset; unknown version → warn + preset; unknown `tool` string → warn + `Tool::Pen` (today `read_stroke` fails the whole sketch; behaviour change with its own test). Shapes (`element.rs::Style`) get `tool` + optional `CustomBrush` too.
- P3: workspace doc `brushes` LoroMap (`BrushId → {name, spec bytes, updated}`) and `assets` map (content hash → PNG ≤ 64 KiB). Strokes always snapshot the spec inline so documents render without the library and editing a library brush never restyles history.
- Wet ink: `WetInk::Begin` gains `spec: Option<Vec<u8>>`; `WetPoint` becomes the full input sample (`x,y,force,t_ms,tilt`), dropping `width`/`nib`; receivers run stages 2/3 with the same spec + seed. Ephemeral channel, pre-release: both ends ship together.

### Core output

One attributed triangle mesh for continuous and stamped strokes:

```rust
pub struct InkVertex { pub pos: [f32; 2], pub uv: [f32; 2], pub opacity: f32 }
pub struct InkMesh { pub vertices: Vec<InkVertex>, pub indices: Vec<u32>, pub style: InkStyle }
pub struct InkStyle { pub color: Rgba, pub blend: Blend, pub overlap: Overlap, pub mask: MaskStyle /* None | Shape{aspect,corner,hardness} | Image(AssetId) */, pub grain: Option<GrainStyle> }
```

- Continuous round tips: lyon as today, vertex constructor reads `position()`, `advancement()` → `u` (arc length), `side()` → `v ∈ {−1, +1}`, `interpolated_attributes()[1]` → opacity.
- Nib tips: `nib_ribbon` generalised: nib thickness `aspect·w`, each point emits its nib rectangle, each segment the hull quad between consecutive nib centrelines; rectangle ends are the caps.
- Stamped: `dabs(spec, tip_states, seed) -> Vec<Dab{x,y,w,h,rot,opacity}>`, 4 verts + 6 indices per dab, `uv ∈ [−1,1]²` in tip space, mask evaluated in the fragment (zoom independent, no re-tessellation). Spacing walks arc length with a carried remainder. `MAX_DABS = 50_000` + `tracing::warn!`.
- `Overlap::Discard` is a renderer contract (write once per pixel per stroke); core guarantees consistent winding and per-stroke `style.overlap`.
- `InkMesh::zoom_independent()` so the iOS cache skips re-buckets for stamped strokes.
- SVG export (`export.rs`): `Stroke::outline_polygon(spec)` (left edge forward, right edge back, cap arcs / nib ends) as one `<path fill-rule="nonzero" fill-opacity=…>`; nonzero fill of one polygon never darkens, so Discard is exact; stamped strokes export their outline at reduced opacity (grain dropped); Multiply → `mix-blend-mode: multiply`.
- FFI: `IndexedMesh` → `InkMesh { vertices: Vec<f32> /* stride 5 */, indices, style }`; `BrushRef { tool, spec: Option<BrushSpec>, base_width, seed }` bundles brush args for `points_mesh`/`wet_mesh`/`stroke_mesh`/`element_mesh`. P3: `LiveInk` object with `mesh_delta() -> { keep_vertices, keep_indices, vertices, indices }`.

### Modeler and dynamics

```rust
pub trait InputModel { fn push(&mut self, raw: RawSample) -> Option<StrokePoint>; fn predict(&self, raw: &[RawSample]) -> Vec<StrokePoint>; fn finish(&self) -> Vec<StrokePoint>; }
pub enum Modeler { Ema(EmaModel), Ism(IsmModel) }   // Ism behind cargo feature `ism`
```

- `EmaModel` = today's `step()` minus width/taper/speed. `finish()` no longer tapers; `DistanceToEnd` does, when `complete = true`.
- `BrushModeler` facade keeps `new(tool, size)`, `push`, `points`, `predict`, `finish`; adds `for_brush(spec, size, seed)` and `tip_states(complete)`. Shape recognition keeps using `points()` (inputs).
- `TipEvaluator::new(spec, base_width, seed)`: per point speed = `dist / max(dt_ms, 1)` damped `exp(−dt/τ)`; each behaviour damped by its `damping_ms`; targets applied onto `w = h = base_width`; `rot` from `Orient`; then rate limit `|Δw| ≤ max_size_rate·Δarc`; floors `w,h ≥ 0.05`, opacity clamped 0..1; `size: Some` overrides `w,h` before opacity dynamics. Only place that reads `Tilt`.
- `IsmModel` (P4) wraps `ink-stroke-modeler-rs` (check licence/MSRV); first trial keeps EMA prediction on top of ISM output.

### Core tests

- `same_inputs_same_bytes` (bit-exact), `live_equals_committed` (point-by-point fold == whole-slice fold, same dabs), translation equivariance (multiples of 1/8), rotation equivariance for round Motion tips, `discard_overlap_polygon_never_darkens`, `SpecRepr` round trip + unknown version tolerated, tolerant `read_stroke` (unknown tool / missing spec / bad bytes), `size_none_for_new_strokes`, dab count bounded / spacing floor / zero-length stroke = one dab, legacy baked-size parity.
- Golden meshes `tests/golden/*.txt` (FNV hash + vertex count; `KRABINK_UPDATE_GOLDEN=1`); cross-platform compare at 1e-4.
- Corpus `tests/corpus/brush/*.txt` (line `x y force t_ms [azimuth altitude roll]`, header `# tool: pencil size: 3`), `tests/brush_corpus.rs` prints jitter / lag / overshoot / point count per model; `examples/replay.rs` writes SVG side-by-sides.
- `cargo mutants -f crates/krabink-core/src/brush/*.rs -f crates/krabink-core/src/geom/*.rs` per phase.

### Core files

`src/brush/{mod,input,dynamics,spec,rng,ism}.rs`, `src/geom/{mod,mesh,continuous,nib,stamps,outline}.rs`, `stroke.rs`, `element.rs`, `note.rs`, `wetink.rs`, `export.rs`, `workspace.rs` (P3), `lib.rs`, `tests/brush_corpus.rs`, `tests/corpus/brush/`, `tests/golden/`, `examples/replay.rs`, `Cargo.toml` (`ism` feature). FFI: `crates/krabink-ffi/src/{brush,types,engine,lib}.rs`.

## Input, tool UX, wire, migration (iOS + desktop)

### Decisions

| Question | Decision | Why |
|---|---|---|
| Force normalisation | `force = min(touch.force / 2, 1)`; fingers/simulator = 0.5 (`forceFullScale = 2.0` Apple-average units, one constant in `RawSample.init`) | Today's `min(force, 1)` discards everything above average pressure. Dividing by `maximumPossibleForce` (4.17) wastes most of the u8 force range. Presets define nominal width at force 0.5. Old strokes carry baked `size`, so reinterpreting force does not change them. |
| watercolor ink | iOS 18: hidden via `PKToolPicker(toolItems:)`. iOS 17: `Marker` at opacity × 0.6, Accumulate | Cannot render watercolour honestly; stores as plain `Marker` so every device agrees. |
| crayon ink | P1–P2: `Pencil` at `base_width × 1.5`. P3: bundled custom brush `builtin:crayon` (stamped + grain) | Keeps the fixed `Tool` enum. |
| Estimated properties after pen-up | Defer commit ≤ 200 ms while pushed samples still expect updates; commit as soon as the pending set empties. Never edit a committed stroke. | No CRDT edit op, no receiver flash; wet ink already lingers 5 s on receivers. Fallback (`replaceStrokePoints`) only if device logs show updates later than 200 ms. |
| Wet points | `WetPoint` deleted; wire carries stage-1 `StrokePoint`s chunk-coded (`encode_chunks`) | Receivers run stage 2/3 on exactly the points the commit will contain → zero change at commit; ~9 B/point with tilt vs ~22 B today. |
| Squeeze | System default | Shows the picker. |
| Colour alpha | Keep alpha packed in `color`; effective opacity = `color.a/255 × spec.paint.opacity` | Compatible with old strokes, desktop, thumbnails. |

### Tool picker (`StrokeCodec.swift`, `SketchScreen.swift`)

| PencilKit ink | Core brush | base_width | opacity × |
|---|---|---|---|
| `.pen` | `Pen` | `w × K.pen` | 1.0 |
| `.pencil` | `Pencil` | `w × K.pencil` | 1.0 |
| `.marker` | `Marker` (Discard, chisel from roll) | `w × K.marker` | `A.marker` |
| `.monoline` | `Monoline` | `w × K.monoline` | 1.0 |
| `.fountainPen` | `Fountain` (nib = azimuth + roll) | `w × K.fountain` | 1.0 |
| `.crayon` | see above | | 0.9 |
| `.watercolor` | see above | | 0.6 |
| `PKToolPickerCustomItem` (iOS 18) | `.custom(id, spec)` from the library | `item.width` | spec |
| `PKEraserTool` | eraser, unchanged | | |

`K.*`/`A.*` start at 1.0 and are measured, not guessed: a `-brushLab 1` calibration page shows a `PKCanvasView` above our canvas with the same tool; a UI test replays the same stroke into both (`PKDrawing` from `PKStrokePoint`s) and scans a pixel column for width and alpha, printing the ratios to copy into the tables.

```swift
struct BrushSelection: Equatable { var brush: BrushRef /* .preset(Tool) | .custom(id, spec) */; var color: UInt32; var baseWidth: Float }
enum StrokeCodec { static func selection(inking: PKInkingTool) -> BrushSelection; @available(iOS 18,*) static func selection(item: PKToolPickerItem, library: BrushLibrary) -> BrushSelection?; static let widthScale: [InkType: Float]; static let opacityScale: [InkType: Float] }
```

`SketchModel.tool: PKTool` → `selection: BrushSelection?` + `eraser`. iOS 18: `PKToolPicker(toolItems:)` (pen, pencil, marker, monoline, fountainPen, crayon, vector eraser, custom items in P3), `stateAutosaveName = "sketch"`. iOS 17: stock picker, watercolor mapped, in-app `BrushSheet` (SwiftUI) for custom brushes with a status pill. P3 custom item: `PKToolPickerCustomItem.Configuration` (identifier, name, colour, width 1…64, `imageProvider` renders one dab + short stroke via the core, `viewControllerProvider` → `BrushAttributesView` with size/opacity/spacing/grain/jitter, persisted in `UserDefaults` `brush.<id>.*`).

### Input capture (`SketchScreen.swift`)

- `RawSample` gains `estimationId: UInt32?` (`estimationUpdateIndex`) and `estimated: UInt8` mask (force 1, azimuth 2, altitude 4). Tilt always captured for pencil (azimuth, altitude, roll iOS 17.5+).
- `PenGestureRecognizer` implements `touchesEstimatedPropertiesUpdated` → `.estimateUpdated([RawSample])`. Core FFI: `BrushModeler.update(estimationId, force?, tilt?) -> Bool` (patches the stored point, re-runs the fold from that index; core keeps `estimationId → point index` while live) and `pendingEstimates() -> [UInt32]`.
- `SketchModel`: on update → `modeler.update` + `showLive`; recorder patches its row. `penEnded`: commit now if no pending estimates, else move the stroke to `settling: [String: LiveStroke]` with a 200 ms task; updates on a settling stroke commit early when the set empties; later updates ignored. Renderer `local` → `locals: [String: GPUGeometry]` so settling strokes stay drawn until `show(element)` replaces them. Log `est: pushed= updated= late= maxLateMs=` at commit.
- Hover (P2, Pencil Pro): `UIHoverGestureRecognizer` on the scroll view → `renderer.setHover(hoverDabMesh(brush, baseWidth, tilt), color, alpha 0.35)`; cleared on pen-down/hover end. Core: `hover_dab_mesh(brush, base_width, tilt) -> InkMesh` = one `TipState` at force 0.5.
- Barrel roll already flows into `Tilt.roll`; `Orient::Nib` reads it per point.

### Wet-ink wire (`wetink.rs`)

```rust
pub enum WetInk {
    Begin  { sketch, stroke, tool: Tool, color: Rgba, base_width: f32, spec: Option<Vec<u8>> /* custom only, SpecRepr bytes */ },
    Points { stroke, seq: u32, sent_ms: u64, chunks: Vec<Vec<u8>> /* encode_chunks of stage-1 StrokePoints since last batch */ },
    End    { stroke, sent_ms: u64, tail: Vec<Vec<u8>> /* points finish() added */ },
    Cancel { stroke },
}
```

FFI: `NoteListener.wetPoints(stroke, sentMs, points: [StrokePoint])`, `NoteSession.appendPoints(stroke, seq, points)`, `wetBegin(..., spec)`; `wetPoints()`/`wetMesh` helpers go, `pointsMesh` serves receivers. `PROTO_VERSION` bump; both peers ship from one tree. Batching (60 ms, ≤ 480, seq) unchanged. Missing assets on a receiver: `AssetResolver` falls back to `Mask::Shape` / no grain, records `waiting[hash]`, re-renders when the workspace `assets` map delivers it. Desktop `apply_wet_ink` does the same via `ink_mesh(brush, points, base_width)`.

### Migration

- Old strokes: `size: Some` → stage 2 override, dynamics skipped. `BSplineControl` flattening unchanged.
- `Tool::Brush` → `Fountain` (`"brush"` read alias); Swift `.brush` → `.fountain`; `scripts/smoke/main.swift` and `crates/krabink-ffi/tests/engine.rs` follow the new wet/mesh signatures. UI tests reference no tool names (verified).
- `note.rs::read_stroke` tolerant to missing `brush`/`spec`; `add_stroke` writes them only when present. Regenerate the Xcode project (`xcodegen`) when Swift files are added; rebuild the xcframework with every FFI change.

### Tuning tooling

- Recorder v2 (`-recordStrokes 1`): header `# krabink-stroke v2`, `# tool: pencil size: 3 color: … brush: preset`, `# device: … pencil: pro ios: … force: half-average zoom: …`, `# columns: x y force t_ms azimuth altitude roll est`; rows written after settling; `nan` tilt for non-pencil. Parser generalised into `krabink_core::corpus::parse` (4- or 7/8-column rows), reused by the shape harness, `tests/brush_corpus.rs` and the lab.
- Desktop lab: `krabink brush-lab --corpus <file|dir> --presets pen,pencil,… --models ema,ism --out target/lab --png --svg --metrics`. Headless bevy (`ScheduleRunnerPlugin` + wgpu `RenderPlugin`), same `SketchScene` camera, `ink_mesh` and material as the app, one cell per (file × preset × model), `Readback::texture` → PNG; metrics (jitter = mean |Δ² pos|, lag = modelled-vs-raw distance at t, overshoot beyond raw end, point count, width variance, wall time) as a table + `metrics.json` with bounds checked in `tests/corpus/brush/metrics.json`.
- iOS `-brushLab 1` (`KrabinkApp.swift`, like `-spike 1`): grid of presets drawn by `InkRenderer` from the same canned corpus stroke, EMA/ISM toggle, calibration page, "open canvas with recorder".

### Editing / eraser

Data model allows patching points and swapping the spec; no UI in this plan. Eraser stays whole-element; partial eraser for stamped brushes noted as future.

## Rendering (Metal + bevy WGSL, lockstep)

Verified bevy 0.19.1 facts that make this cheap on desktop: 2D always has a `Depth32Float` attachment per camera at the camera's MSAA count, reversed-Z cleared to 0.0, stored across the transparent pass (`bevy_core_pipeline/src/core_2d/mod.rs:47,424-466`); `Material2d::specialize` can overwrite blend, depth_stencil and vertex buffers; `Transparent2d` sorts by z so draw order == z order and consecutive same-pipeline/material items batch; `MeshTag(u32)` is readable in WGSL via `mesh2d_functions::get_tag`; `Screenshot::image(handle).save_to_disk` dumps offscreen targets; `Image::reinterpret_stacked_2d_as_array` turns a stacked PNG into a texture array.

### Per-stroke style buffer + per-vertex stroke index

Geometry rides per vertex exactly as core emits it; everything per stroke lives in a `StrokeStyle` array. Only real pipeline state (blend × overlap = 4 combos) splits a draw.

```
struct StrokeStyle {               // 64 B, identical bytes on Metal and WGSL
    float4 color;                  // linear RGBA, straight
    float4 mask;                   // aspect, corner, hardness, 0
    float4 grain;                  // scale, strength, seed, 0
    float  depth;                  // Metal: NDC z slot; bevy: unused (Transform z)
    uint   flags;                  // bits 0-1 MASK_KIND (none/shape/image), 2-3 GRAIN (none/noise/image), 4 GRAIN_MAPPING (canvas/stroke), 5 MULTIPLY, 6 DISCARD
    uint   maskLayer; uint grainLayer;
};
```

- **Metal vertex streams:** buffer 0 stride 20 = core `InkMesh.vertices` uploaded verbatim (`float2 pos`, `float2 uv`, `float opacity`); buffer 3 stride 4 = `uint stroke` index; buffer 1 `Uniforms{scale, translate, zoom}`; buffer 2 `StrokeStyle[]`. The Swift per-vertex conversion loop disappears.
- **Runs:** batch index buffer stays in z order; `rebuildBatch()` records `runs: [(combo, indexOffset, indexCount)]`, new run when (blend, overlap) changes. Draw calls = runs (tens on a full page), same buffers bound, only PSO/depth-state switches. Order must equal z order for translucent correctness, so runs are not merged across differing combos (optional later: merge when AABBs do not intersect).
- **bevy:** one entity per stroke as today (`Mesh2d` + `MeshMaterial2d<InkMaterial>` + `Transform` z + `MeshTag(style_index)`), attributes `ATTRIBUTE_POSITION` + `Ink_Uv (Float32x2)` + `Ink_Opacity (Float32)`. Four `InkMaterial` assets per sketch (one per combo) sharing one `Handle<ShaderStorageBuffer>` of `StrokeStyle`; bevy's sorted-phase batching yields the same runs.

### Self-overlap Discard: depth trick

- Every stroke gets a depth slot strictly monotone in z order; draw order == z order.
- **Discard:** strict compare (`.less` Metal / `Greater` bevy reversed-Z), depth write on. First fragment of a stroke at a sample writes its depth; later fragments of the same stroke (equal depth) are rejected; later strokes are strictly nearer and blend over.
- **Accumulate:** inclusive compare (`.lessEqual` / `GreaterEqual`), write on. Same-stroke fragments pass and accumulate.
- MSAA: depth per sample, outer-edge coverage AA untouched, shared triangle edges give neither double hits nor seams.
- Fragments with `a < 1/255` `discard` on both platforms (dab quad corners, feather tails) so they never stamp holes.
- Contract with core: `Overlap::Discard` only with hard masks (`None` or `Shape` hardness ≈ 1); soft dabs + Discard would scallop. First-drawn segment wins at self-crossings (same as Google Ink).

| combo | PSO | Metal depth | bevy depth |
|---|---|---|---|
| Normal + Accumulate | `psoNormal` | `.lessEqual`, write | `GreaterEqual`, write |
| Normal + Discard | `psoNormal` | `.less`, write | `Greater`, write |
| Multiply + Accumulate | `psoMultiply` | `.lessEqual`, write | `GreaterEqual`, write |
| Multiply + Discard | `psoMultiply` | `.less`, write | `Greater`, write |

Metal: `metal.depthStencilPixelFormat = .depth32Float` (MTKView allocates a memoryless 4x depth texture, clears to 1.0), `depthAttachmentPixelFormat` on both PSOs, two `MTLDepthStencilState`s. Depth slots baked into `StrokeStyle` at batch rebuild: committed k of N → `0.2 + 0.8·(N−k)/(N+1)`, remote wet j → `0.1 + 0.001·(W−j)`, local live `0.01`. bevy: `specialize` sets `depth_write_enabled = Some(true)` and `depth_compare` per key; Transform z committed `k·0.01`, remote wet `990 + 0.01·j`.

### Fragment über-shader, premultiplied alpha, linear blending

- **Colour space:** blend in linear on both. iOS switches the MTKView to `.bgra8Unorm_srgb` (today it blends in gamma while bevy blends in linear, so marker overlaps already differ); CPU converts `0xRRGGBBAA` to linear for `StrokeStyle.color`; background clear converted through the sRGB EOTF. Existing iOS marker overlaps change slightly: intended.
- **Output premultiplied** `(rgb·a, a)`.

| blend | Metal (rgb / alpha) | wgpu `BlendState` |
|---|---|---|
| Normal | `.one` / `.oneMinusSourceAlpha`, same for alpha | `PREMULTIPLIED_ALPHA_BLENDING` |
| Multiply | rgb `.destinationColor` / `.oneMinusSourceAlpha`; alpha `.one` / `.oneMinusSourceAlpha` | color `{Dst, OneMinusSrcAlpha, Add}`, alpha `{One, OneMinusSrcAlpha, Add}` |

Multiply: `out = lerp(dst, c·dst, a)`, the classic highlighter; needs its own PSO (no dst read in WGSL, and framebuffer fetch would break parity).

```metal
fragment float4 ink_fragment(V2F in, constant Uniforms& u, constant StrokeStyle* styles,
                             texture2d_array<float> masks, texture2d_array<float> grains, sampler clampMip, sampler repeatMip) {
    StrokeStyle s = styles[in.stroke];
    float m = 1.0;
    switch (s.flags & 3u) {
      case 1u: { float r = s.mask.y; float2 q = abs(in.uv) - (1.0 - r);          // rounded superellipse in tip space [-1,1]²
                 float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
                 float w = max(fwidth(d), 1.0 - s.mask.z);                          // ≥1 px AA, wider when soft
                 m = 1.0 - smoothstep(-w, 0.0, d); break; }
      case 2u: m = masks.sample(clampMip, in.uv * 0.5 + 0.5, s.maskLayer).r; break;
    }
    float g = 1.0;
    if ((s.flags >> 2) & 3u) {
        float2 p = ((s.flags & 16u) ? in.uv : in.canvas) / s.grain.x;              // Stroke: (u/scale, v); Canvas: page-locked
        float n = ((s.flags >> 2) & 3u) == 1u ? value_noise(p, s.grain.z) : grains.sample(repeatMip, p, s.grainLayer).r;
        g = mix(1.0, n, s.grain.y * saturate(s.grain.x * u.zoom / 1.5));           // fade noise below 1.5 px per cell
    }
    float a = s.color.a * in.opacity * m * g;
    if (a < 1.0 / 255.0) discard_fragment();
    return float4(s.color.rgb * a, a);
}
```

`value_noise` = bilinear interpolation of a u32 integer hash (pcg2d) of `floor(p) + seed`: bit-identical across Metal and WGSL. WGSL (`crates/krabink/src/ink.wgsl`, `embedded_asset!`) has the same body; vertex uses `mesh2d_functions::{get_world_from_local, mesh2d_position_local_to_clip, get_tag}`; bindings on the material group: styles storage (0), masks + sampler (1,2), grains + sampler (3,4), `InkParams{zoom}` uniform (5, 16-B padded). `discard` is allowed under `AlphaMode2d::Blend`.

```rust
#[derive(Asset, AsBindGroup, Clone, TypePath)] #[bind_group_data(InkKey)]
pub struct InkMaterial {
    #[storage(0, read_only)] pub styles: Handle<ShaderStorageBuffer>,
    #[texture(1, dimension = "2d_array")] #[sampler(2)] pub masks: Handle<Image>,
    #[texture(3, dimension = "2d_array")] #[sampler(4)] pub grains: Handle<Image>,
    #[uniform(5)] pub params: InkParams,
    pub multiply: bool, pub discard: bool,
}
impl Material2d for InkMaterial { /* vertex/fragment "embedded://krabink/ink.wgsl", alpha_mode Blend, specialize: vertex buffers (position, Ink_Uv, Ink_Opacity), blend per key, depth write + compare per key */ }
```

Confirm in P1's first hour that `#[storage(0, read_only)]` accepts `Handle<ShaderStorageBuffer>` on 0.19.1.

### Buffers, assets, samplers

- Metal `GPUGeometry` → `{vertices, strokeIndex, indices, styles, runs}` with today's geometric growth/reuse; wet and live own one-entry style buffers. `rebuildBatch()` on `batchDirty` only.
- `InkAssets`: two `r8Unorm` mipmapped 2D texture arrays, `masks` (256², clamp) and `grains` (512², repeat), from bundled stacked PNGs `ios/Krabink/Resources/ink/{masks,grains}.png` (add `resources:` in `project.yml`) via `MTKTextureLoader` → blit slices → `generateMipmaps`; two samplers `clampMip`, `repeatMip`. `assetId → layer` table shared with core asset ids. bevy: same PNGs via `include_bytes!` → `Image::from_buffer` → `reinterpret_stacked_2d_as_array`, `ImageSampler::Descriptor` (repeat / clamp, linear mips), `usage |= TEXTURE_BINDING`; CPU box-filter mips if the loader does not generate them (shared ~40-line helper).
- bevy style storage: `Assets<ShaderStorageBuffer>` per sketch, stable index per element via a free-list in `SketchScene`, rewritten on element diff.

### Zoom

Continuous strokes keep bucket re-tessellation; stamped strokes carry `zoom_independent` on `InkMesh` and `retessellateCommitted()` skips them. Grain `Canvas` samples canvas coordinates, so paper zooms with the page, overlapping strokes reveal the same paper, and iOS at zoom 1 equals desktop pixel-for-pixel; textures are mipmapped, procedural noise fades below 1.5 px per cell. Screen-locked grain rejected (breaks parity and thumbnails).

### Live stroke and pen-up swap

`setLocal(mesh)` uploads with depth slot 0.01 and a one-entry style; run PSO/depth chosen from `style`. Pen-up `show(element)` gets identical geometry from core (same triangle order → same Discard resolution), only the depth slot changes. P4: `GPUGeometry.append/truncate` for the incremental delta API; stroke stream stays zero.

### Thumbnails: Metal, retire CoreGraphics

CoreGraphics cannot do masks, grain, linear multiply or Discard. New `InkRenderer.renderThumbnail(elements:, maxSide:) -> UIImage`: offscreen `bgra8Unorm_srgb` 2× target with 4x MSAA colour + `depth32Float` depth and a resolve target, same `rebuildBatch` path at the thumbnail tolerance, `waitUntilCompleted`, `getBytes` → `CGImage`. `SketchPreview` keeps `thumbCache`/`framed`/placeholder; `IndexedMesh.cgPath` deleted. Desktop thumbnails are the live scene already.

### Rendering files

`ios/Krabink/Sources/InkRenderer.swift`, `Shaders.metal`, `SketchScreen.swift` (MTKView formats), `SketchPreview.swift`, new `InkAssets.swift`, `ios/Krabink/Resources/ink/*.png`, `project.yml`; `crates/krabink/src/sketch.rs`, new `ink_material.rs`, `ink.wgsl`; `crates/krabink-ffi/src/brush.rs` (`InkMesh` + `zoom_independent`).

## Phases (each ships core + FFI + iOS + desktop together)

| Phase | Ships | Core/FFI | iOS | Desktop | Days |
|---|---|---|---|---|---|
| **P0** split stages, no visible change | `brush/{input,dynamics,spec,rng}`, `geom/{mesh,continuous,nib,stamps,outline}`, `TipEvaluator` reproducing today's width formula, golden parity on existing corpus | 1.5 | 0 | 0 | 1.5 |
| **P1** tip shapes, opacity, marker Discard | `BrushSpec` + presets, `Tool::{Pencil,Fountain}`, `Stroke.brush`, loro `brush`/`spec`, tolerant reader, nib ribbon with thickness + rectangle caps, `InkMesh`/`InkStyle`/`BrushRef` across FFI, `WetInk` v2 (Begin.spec, chunked points, End.tail), `outline_polygon` export, `BrushModeler.update/pending_estimates` | FFI rename + smoke/engine tests, `StrokeCodec` selection + K/A tables, `SketchModel.selection`, picker `toolItems` (iOS 18) + watercolor rule, force normalisation, always-on tilt, estimated properties + settling commit + `locals`, two-stream vertex + style buffer + runs, depth attachment + 2 depth states, Normal/Multiply PSOs, sRGB framebuffer + premultiplied, Metal thumbnails, recorder v2, `-brushLab` skeleton + calibration page | `InkMaterial` + `MeshTag` + storage buffer + `specialize`, `ink_mesh(brush, …)`, wet receiver on chunked points, replay rig | 4–5 core + 5 iOS + 3 desktop ≈ **12** |
| **P2** pencil | tilt/pressure behaviours, `Grain{Noise, Canvas}` → `InkStyle`, hardness feather, corpus with tilt columns, `hover_dab_mesh`, `corpus.rs` parser, `tests/brush_corpus.rs` | hover recogniser + hover layer, pencil/crayon wiring, device recordings (6–8), settling UI test | Shape SDF mask + integer-hash noise in WGSL, `zoom_independent` | 2 core + 3 iOS + 1.5 desktop ≈ **6.5** |
| **P3** stamps + custom brushes | `Emit::Stamped` dabs + PCG streams, `Mask::Image`/`GrainSource::Image`, workspace `brushes` + `assets` maps, library CRUD FFI, `LiveInk` delta, "Pencil (grainy)", `builtin:crayon` | `BrushLibrary`, `PKToolPickerCustomItem` + attribute view + icon provider + `UserDefaults`, iOS 17 `BrushSheet` + status pill, asset import (greyscale PNG ≤ 64 KiB, content hash), receiver fallback + re-render on asset arrival, texture arrays + mips + samplers, `GPUGeometry.append/truncate`, stamped thumbnails, round-trip UI test | texture arrays + mips, asset resolver, library sync | 4–5 core + 7 iOS + 2.5 desktop ≈ **14** |
| **P4** ISM trial | `IsmModel` behind `ism` feature, corpus metrics, decision memo in `docs/plans/brush-engine.md` | `-brushLab` EMA/ISM toggle, ISM tilt mapping, corpus replay on device | `krabink brush-lab` headless CLI (PNG/SVG grid, `metrics.json`), `--dump-sketch` | 2–3 core + 1 iOS + 2 desktop ≈ **5.5** |

Total ≈ 40 working days. P0+P1 is the first shippable slice (proper highlighter, chisel, calligraphy caps, no self-darkening, parity fix on iOS colour space).

## Verification

- **Core:** `cargo test --workspace`, clippy `-D warnings`, fmt, ast-grep rust rules on new files, `cargo mutants -f crates/krabink-core/src/brush/*.rs -f crates/krabink-core/src/geom/*.rs` per phase (install `cargo-mutants`). Golden meshes; corpus metrics bounds; `same_inputs_same_bytes`; `live_equals_committed`; P0 parity within 1e-4.
- **FFI/Swift:** `cargo test -p krabink-ffi` (relay test with the new wet signatures), `scripts/swift-smoke.sh`, `scripts/build-ios-core.sh`, `xcodegen generate` when Swift files change.
- **iOS simulator:** existing UI tests unchanged (relay `krabink-server --listen 127.0.0.1:8722 --db … --token demo`, `KRABINK_TEST_SERVER`/`KRABINK_TEST_TOKEN`, arm64 destination per memory) plus `testMarkerSelfOverlapDoesNotDarken` (figure-eight drag, centre-pixel check), `testEstimateUpdateDoesNotDuplicateStroke`, calibration test (log-only until first values are committed), P3 `testCustomBrushStrokeRoundTrips`; screenshots via `xcrun simctl io <udid> screenshot`.
- **Desktop:** `krabink replay` on the desktop with chunked points; `--dump-sketch <id> <png>`; `scripts/ink-parity.sh` crops an iOS zoom-1 screenshot to the sketch bounds and `compare -metric RMSE` against the desktop PNG, threshold ≤ 1.5 %.
- **Device checklist per phase** (`devicectl` install + launch with console, per memory): first-frame log extended with tool/K/alpha; `est:` line at commit (late updates > 200 ms → implement `replaceStrokePoints` fallback); recorder v2 header; highlighter over pen shows multiply, self-crossing not darker; chisel/nib rotate with barrel roll live; hover dab on Pencil Pro; picker state survives relaunch; pinch-zoom on a page of stamped strokes does not re-tessellate.

## Risks and calls

- Bit-exact cross-platform meshes are impossible through libm; contract is bit-exact per binary, 1e-4 across platforms, integer-only randomness.
- `Overlap::Discard` needs the renderer write-once path; land core P1 and both renderers in the same release. Discard + soft masks unsupported by construction.
- iOS sRGB framebuffer switch changes existing marker overlaps (intended parity fix). `discard_fragment` disables early-Z on ink pipelines (negligible for 2D ink).
- `WetPoint` removal breaks old receivers against new senders: pre-release, ephemeral channel.
- Speed from integer `t_ms` is coarser than raw dt; damping hides it; a chunk V2 at 0.25 ms only if the corpus shows stepping.
- `ink-stroke-modeler-rs` licence/MSRV to check before wiring; it stays feature-gated and unexposed until the P4 decision.
- bevy `ShaderStorageBuffer` mutation re-prepares the bind group on commit, not per frame.

### iOS / desktop files

`ios/Krabink/Sources/SketchScreen.swift` (RawSample, recogniser phases, settling, selection, recorder v2, picker), `StrokeCodec.swift` (selection + tables), `InkRenderer.swift` (locals, hover layer, `InkMesh`), `AppModel.swift` (listener signatures, library), `SketchPreview.swift` (stamped thumbnails), `KrabinkApp.swift` (`-brushLab`); new `BrushLibrary.swift`, `BrushSheet.swift`, `BrushAttributesView.swift`, `BrushLabScreen.swift`, `AssetImport.swift`, `UITests/BrushLabUITests.swift`. Core/FFI/desktop: `wetink.rs`, `brush/*` (update/pending estimates), new `corpus.rs`, `tests/brush_corpus.rs`, `krabink-ffi/src/{brush,types,engine}.rs`, `krabink-ffi/tests/engine.rs`, `scripts/smoke/main.swift`, `crates/krabink/src/{sketch,cli,main,replay}.rs`, new `lab.rs`.

## Implementation notes

### P0 (done, `fa20095`)

`brush.rs`/`geom.rs` split into `brush/{input,dynamics,spec}` and
`geom/{mesh,continuous,nib,outline}` with a golden-mesh test
(`tests/golden_mesh.rs`, `KRABINK_UPDATE_GOLDEN=1` to regenerate) pinning
the output.

### P1 (done, core `9394204`, iOS `b2fa99d`, desktop follows)

Core/FFI as designed, with these calls:

- `InkStyle` carries `hardness` only; mask aspect/corner and grain fields
  are reserved in the renderer's `StrokeStyle` (mask kind 3 = ribbon edge
  feather on `|v|`) until P2 stamps need tip-space masks.
- Pen preset maps force 0.5 (average pressure after the `/2` normalisation)
  to 75 % width, not 100 %: the `K.*` calibration tables absorb the rest.
- `BrushModeler.update` patches a stored point and re-runs the fold from
  it; `pending_estimates` drives the 200 ms settle on the iPad.
- Wet `End` tail is delivered to listeners as one more `wet_points` batch
  followed by `wet_end`; receivers finish the mesh with `StrokeEnd.complete`.

iOS:

- Renderer draws runs split only by blend × overlap; depth slots committed
  `0.2 + 0.8·(N−k)/(N+1)`, wet `0.1 + 0.001·(W−j)`, live `0.01`, settling
  `0.05 − 0.001·i`. Thumbnails are the same pipeline offscreen (2×, MSAA 4,
  resolve → `CGImage`). `IndexedMesh.cgPath` is gone.
- `-tool <name>` pins the tool and skips the picker; `-figureEight 1`
  commits a lemniscate; `testMarkerSelfOverlapDoesNotDarken` samples
  screenshot pixels at the crossing vs an arm (passes: uniform grey).
- `-brushLab 1`: preset grid (5 tools × 3 widths from one canned stroke)
  and a calibration page with `PKCanvasView` above our canvas. The ratio
  measurement is still by eye; `widthScale`/`opacityScale` stay at 1.0
  (crayon 1.5 / 0.9, watercolour 0.6 by rule).
- Recorder v2 writes tilt columns and an `est` flag (1 = still an
  estimate at commit); the shape corpus parser reads the first four
  columns and ignores the rest, so recordings still replay there.
- Picker state autosaves as `sketch` (iOS 18 `toolItems` without
  watercolour; iOS 17 stock picker with watercolour → lighter marker).

Desktop: `ink_material.rs` + `ink.wgsl`. bevy 0.19 names the storage asset
`ShaderBuffer`; `specialize` reaches `depth_stencil`, so write-once ink
works as designed. `PreparedMaterial2d` does not re-prepare after a
`ShaderBuffer` reallocation, so the palette pads to a power of two and
re-touches its four materials for two frames after growth.

Device follow-up (`b5d1060`, `01214c5`): the fountain pen lagged because
every estimate update re-meshed through element-wise UniFFI lifts; the
live mesh is now built inside the modeler (`BrushModeler.live_mesh`),
meshes cross the FFI as bytes, and estimate redraws coalesce to one per
run-loop pass (≤ 2 ms at 1000 points). Barrel roll turned the nib the
wrong way: `Tilt::nib_angle` is `azimuth − roll`. Estimates land within
~25 ms of pen-up on an iPad Pro M4.

Not done in P1: the automated K/A calibration, `cargo mutants`, and the
desktop parity screenshot script.

### P2 (done)

Core/FFI:

- `Paint.grain: Option<Grain { source: Noise, mapping: Canvas | Stroke,
  scale, strength }>`; `SPEC_VERSION` is 2 (no custom specs were stored
  under 1). `InkStyle.grain: Option<GrainStyle>` carries it to the
  renderers with the hash seed resolved: 0 for canvas-mapped grain (every
  stroke reveals the same paper), the element id's low 32 bits
  (`ElementId::seed`, `Ink.seed`, `BrushRef.seed`, `stroke_seed()`) for
  stroke-mapped. Pencil preset: noise, canvas, scale 1.5, strength 0.55.
- The seed rides in the style's `grainLayer` slot (an integer; `grain.z`
  as `f32` would lose bits), so both shaders take `value_noise(p, uint)`.
- `Ink::hover_dab(x, y, tilt, tolerance)` / FFI `hover_dab_mesh`: the
  one-point mesh at force 0.5, a dot for round tips, the nib rectangle for
  oriented ones.
- `krabink_core::corpus::parse` reads v1 (4 columns) and v2 (7/8 columns,
  `nan` tilt, `est` flag) recordings; the shape corpus uses it.
  `tests/corpus/brush/` holds fifteen iPad recordings with tilt and roll
  (six fountain, six pencil at size 45 with altitude 0.75–1.03 rad, one
  each pen, marker, monoline); `tests/brush_corpus.rs` prints points,
  jitter (second difference over mean step), lag, overshoot, width range
  and vertex count per file and asserts loose bounds (lag ≤ 4 sizes,
  overshoot 0, jitter within 1.5× raw). Pencil widths come out 1.2–1.6×
  the base width: the tilt behaviour is live on real data.
- Tilt/pressure behaviours and the hardness feather were already in the
  P1 presets and shaders; P2 only added grain on top.
- Pencil is `Overlap::Discard` (`64b84c9`, then this): a crayon at 140
  units wide doubling back on itself stacked hundreds of join fans, and
  accumulating ink lit them up. Write-once ink makes any self-overlap of
  the stroker invisible. The soft edge under Discard is a stipple rather
  than a feather: in the outer band the fragment is dropped where a
  0.75-unit white-noise cell falls below its band position, so every
  drawn fragment carries the full alpha and overlaps show no seams. Also
  points closer than a fifth of the base width are merged before
  tessellation (`SEGMENT_FRACTION`), which removes the sub-unit
  reversals a still pen produces.

iOS:

- `StrokeStyle` sets grain kind 1 + stroke flag + `grainLayer` seed from
  `InkStyle.grain`; nothing else changed in the pipeline.
- Hover: a `UIHoverGestureRecognizer` (pencil touch type) on the scroll
  view feeds `SketchModel.hover`, which draws `hoverDabMesh` at 35 % of
  the ink's alpha in a dedicated `hover` geometry at depth 0.005; cleared
  on pen-down, hover end, or with the eraser selected.
- `-fakeEstimates 1`: finger samples claim a pending estimate that the
  model revises 60 ms after pen-up, so the settling path runs on the
  simulator; the status label shows `est=<revised points>` and
  `testEstimateUpdateDoesNotDuplicateStroke` asserts one stroke, revised,
  then a second one.

- Highlighter on dark paper: `Blend::Multiply` means "keep the text
  legible", and multiply over black is black. The iOS renderer picks the
  pipeline from the clear colour's linear luminance (`darkPaper`):
  multiply on light paper, screen (`src·(1 − dst) + dst`) on dark, for
  the view and thumbnails alike. The desktop's paper is white, so it has
  only the multiply pipeline until it gets a dark theme.

Desktop: `StrokeStyle::from(InkStyle)` fills the grain slots; the WGSL
noise path was already there.

Found, not fixed (recogniser, not ink): proptest
`clean_shapes_are_recognised_equivariantly` fails for a vertical
line path scaled and moved (`+cc e9bd2def1d2e6c6de68c1d91b0825dbf886186ce8df06a0d32c6d2819beeb32c # shrinks to path = [ …`,
seed in the shrunk case): the recognised bounds land ~25 units off the
expected copy. Reproduces on `db50e38`, before any P2 geometry change; the
regression entry was not kept so the suite stays green.

Not done in P2: the Pencil Pro hover check on hardware, and
`InkMesh::zoom_independent` (no stamped meshes exist before P3, so
nothing to skip yet).

### P3 (done, except image assets)

Core/FFI:

- `BrushSpec.emit: Emit { Continuous | Stamped(Stamped { spacing,
  scatter, rotation_jitter, size_jitter, opacity_jitter }) }`;
  `SPEC_VERSION` is 3 (no custom specs were stored under 2).
  `geom/stamps.rs` walks arc length with a carried remainder, one quad per
  dab with `uv ∈ [-1, 1]²` in tip space, tip state interpolated between
  the input points, `Orient::Motion` dabs turned across the path;
  `MAX_DABS = 50_000` truncates with a warning, spacing floors at 0.02
  sizes. `brush/rng.rs` is a murmur3-finaliser hash of (seed, dab index,
  channel): no state, so the live prefix lays exactly the committed dabs
  (`jitter_is_a_function_of_seed_and_index`).
- `InkStyle.mask: MaskStyle { Ribbon | Shape { corner } }` tells the
  renderers which mask to run (kind 3 or kind 1); `InkMesh.zoom_independent`
  is set on stamped meshes and the iOS renderer skips them when the zoom
  bucket changes. Under `Overlap::Discard` the shape mask stipples its
  soft band the way the ribbon does, so stamped write-once ink has no
  scalloped overlaps.
- Bundled brushes (`BrushSpec::builtins()`, `builtin:crayon` and
  `builtin:pencil-grainy`, both stamped + Discard + canvas noise) with
  `BrushKnobs` (`knobs()` / `with_knobs()`): the eight numbers an editor
  shows, applied without knowing the spec layout; setting a stamp knob on
  a continuous brush makes it stamped, a grain knob on flat ink adds noise.
  Golden meshes gained `crayon-s-curve`, `crayon-dot`,
  `pencil-grainy-hairpin`.
- Workspace `brushes` LoroMap (`BrushMeta { id, name, spec bytes,
  updated_ms }`, `upsert_brush` / `remove_brush` / `brushes()`), FFI
  `list_brushes` / `upsert_brush` (rejects undecodable specs) /
  `remove_brush`, `CoreListener.brushes_changed` fired with every workspace
  import; the relay test round-trips an add and a removal between two
  cores.
- FFI: `BrushRef.custom: Option<CustomBrush { id, spec }>` (default
  `None`), `BrushModeler.for_brush(brush)` so a custom brush's input
  smoothing applies, `Stroke.brush`, `builtin_brushes()`, `brush_knobs` /
  `brush_with_knobs`, `begin_stroke(…, spec)` and `wet_begin(…, spec)`
  carry the spec on the wire (the desktop receiver decodes it and draws
  with it).

iOS:

- `SketchModel.picked: PickedTool { ink(BrushSelection) | eraser }`
  replaces the `PKTool`; the coordinator maps inking, eraser and
  `PKToolPickerCustomItem`s through `StrokeCodec`. PencilKit's crayon is
  `builtin:crayon` (`StrokeCodec.custom`), so every crayon stroke stores
  its spec and `custom=` in the status label counts them
  (`testCustomBrushStrokeRoundTrips`: draw, leave, reopen).
- `BrushLibrary` (bundled + workspace brushes, updated by
  `brushesChanged`) builds one `PKToolPickerCustomItem` per brush PencilKit
  has no ink for: the icon and the width variants are the brush itself
  rendered through `InkRenderer.renderThumbnail`; the attributes popover is
  `BrushAttributesView`, sliders over `BrushKnobs`, persisted in
  `UserDefaults` `brush.<id>.knobs` and re-read at pen-down
  (`BrushSelection.refreshed`). The picker's item list is built when the
  canvas is made, so a brush that arrives from a peer shows after the
  sketch is reopened.
- Brush lab rows for the two bundled brushes.

The popover's "Save to library" writes the current knobs as a new `user:`
brush to the workspace (and "Delete from library" removes one); there is
no separate brush editor beyond the knobs.

Image assets (second P3 slice):

- `AssetId::of(png)` is `a:` + FNV-1a 64 of the bytes, so the same
  picture is one asset everywhere and a stroke names exactly the pixels
  it was drawn with. `Asset { id, name, kind: Mask | Grain, png }`,
  `Asset::from_png` checks the PNG signature and `MAX_ASSET_BYTES` (64 KiB).
  `Tip.mask: Mask { Shape | Image(AssetId) }` (images only apply to
  stamped brushes), `GrainSource::Image(AssetId)`; `SPEC_VERSION` 4.
  `InkStyle.mask` gains `Image { asset, corner }`, `GrainStyle.image`.
  Two PNGs are embedded in the core (`assets/paper.png` 256² tileable
  grain, `assets/chalk.png` 128² rough disc) and `builtin:chalk` samples
  both with rotation jitter π.
- Workspace `assets` LoroMap (`AssetMeta { asset, added_ms }`,
  `put_asset` is idempotent on the id, `asset_ids()` is the cheap poll),
  FFI `list_assets` / `put_asset` / `remove_asset`, `builtin_assets`,
  `brush_assets(spec)`, `brush_with_mask`, `brush_with_grain_image`,
  `CoreListener.assets_changed`; the relay test syncs an add and a
  removal and rejects junk bytes.
- Renderers: two `r8` texture arrays, masks 256² clamped and grains 512²
  repeating, both mipmapped (desktop: CPU box mips in `ink_assets.rs`
  with the `image` crate decoding; iOS: `InkAssets` draws the PNG into a
  grey `CGContext` and `generateMipmaps`). Style flags: mask kind 2 with
  `maskLayer`, grain kind 2 (bit value 8) with `grainLayer`; an asset the
  arrays lack falls back to shape / noise at style-build time. Under
  Discard an image mask keeps a fragment with probability equal to its
  coverage (same stipple hash as the edges). The desktop restyles every
  stroke of a scene when the asset generation changes; iOS marks the batch
  dirty and re-meshes wet strokes on `InkAssets.didChange`. The WGSL
  samples both arrays in uniform control flow before the mask switch.
- iOS popover: "tip mask" and "paper" menus over the assets of each kind
  (plus shape / noise), persisted per brush as `brush.<id>.mask|grain`,
  and "Import…" which files a picked picture into the workspace as a
  greyscale PNG shrunk until it fits 64 KiB.

iOS 17: `BrushSheet` (a "brushes" button in the sketch toolbar, shown
only below iOS 18) lists the library with each brush drawn as its icon;
picking one sets the pen to it at the picker's last colour and width and
a pill names it; any PencilKit tool switches back. Compiled, not run: the
simulator and the iPad are on 18.

Not done in P3: the `LiveInk` mesh delta (`GPUGeometry.append/truncate`):
a stamped stroke at spacing 0.15 re-uploads a few thousand quads per
frame, which has not shown up in the redraw timings.

### P4 (done): ISM trial and decision

Core:

- `brush/ism.rs` (feature `ism`, `ink-stroke-modeler-rs` 0.1, MIT/Apache,
  MSRV 1.85) wraps the spring-mass `StrokeModeler`. Positions stay in
  canvas units (the model is linear in position, so only the wobble speed
  window and the stopping distance carry a unit; `IsmParams::units_per_cm`
  = 64 scales the suggested cm/s values). Time is seconds from the first
  sample; a clock that runs backwards is clamped, an identical input is
  skipped, a pause longer than the engine takes in one `update` (~105 ms
  at 180 Hz × 20 outputs) is bridged with synthetic "held" moves so
  `TooFarApart` never fires. Tilt is not modelled by ISM: it is carried by
  interpolating between the previous and current raw sample over the
  modelled times (azimuth the short way round). `predict` runs the EMA
  rule from the last modelled point (the engine has no `Clone`, so it
  cannot be run ahead without committing). `finish` sends `Up` one output
  period after the last sample and then lands the stroke on the pen (the
  spring gets at most 20 settling steps, a fast lift beats it).
- `InputModelKind { Ema, Ism }` + `Modeler` enum behind `BrushModeler`
  (`with_model`, `model()`, `ism_available()`, `InputModelKind::{name,
  parse, available}`). `push` now returns `Vec<StrokePoint>` (ISM upsamples
  a slow pen), `finish` takes `&mut self` and appends the model's tail.
- `corpus::StrokeMetrics::{measure, measure_with}` (points, raw jitter,
  jitter, lag = distance to the raw sample nearest in time, deviation =
  distance to the raw polyline, overshoot, width range, vertices, µs) and
  `corpus::model_points`; `tests/brush_corpus.rs` measures every model the
  build has and `KRABINK_METRICS_JSON=path` dumps the rows.

FFI/iOS/desktop:

- `InputModel` enum, `BrushModeler.for_brush_with(brush, model)`,
  `ism_available()`, `parse_recording(text) -> Recording`,
  `measure_recording(recording, brush, model) -> StrokeMetrics`.
  `scripts/build-ios-core.sh` builds the iOS core with `--features ism`.
- Brush lab: an EMA/ISM segmented control on the presets page and a new
  "corpus" page that replays the bundled recordings (the corpus folder is
  copied into the app as `brush/`) through every model, raw path in grey
  under each, metrics line above; `-labPage 0|1|2` and `-labModel ema|ism`
  preselect. `testBrushLabReplaysCorpus` asserts both models report.
- `krabink brush-lab --corpus <file|dir> [--presets self,pen,…,builtin:x]
  [--models ema,ism] [--out dir] [--svg] [--metrics]` (desktop feature
  `ism` for the ISM row): one SVG grid per recording via
  `elements_to_svg` (columns presets, rows models, raw path under each) and
  `metrics.json`. Not done: the headless bevy PNG path and
  `--dump-sketch`; the SVG comes from the same outline code the export
  uses, which was enough to judge the models.

Decision memo (fifteen iPad recordings, debug build, `KRABINK_METRICS_JSON`):

| tool / stroke | model | points | jitter | lag | deviation | µs |
|---|---|---|---|---|---|---|
| fountain slow calligraphy (6 files) | EMA | 195–690 | 0.19–0.35 | 0.3–0.8 | 0.03–0.04 | 40–130 |
| | ISM | +10–15 % | 0.07–0.13 | 2.8–9.1 | 0.10–0.28 | 2.2× |
| pencil size 45, fast (5 files) | EMA | 126–289 | 0.08–0.13 | 2.3–4.2 | 0.05–0.09 | 20–70 |
| | ISM | +12 % | 0.09–0.14 | 25–47 | 0.6–1.3 | 2× |
| marker / monoline / pen, fast | EMA | 73–468 | 0.11–0.18 | 1.6–4.5 | 0.03–0.14 | 15–100 |
| | ISM | +12–20 % | 0.13–0.16 | 8–41 | 0.4–1.1 | 2× |

- ISM halves the residual jitter on slow strokes (the wobble smoother is
  doing what it is for) and leaves fast strokes unchanged.
- ISM trails a fast pen by 25–47 canvas units at pencil speed: that is the
  spring's time constant (~15 ms at the suggested drag), and it is what the
  library's own predictor exists to hide. Without that predictor wired
  (it needs the engine to be cloneable or a `&mut predict`) the live stroke
  would visibly hang behind the Pencil, which is the one thing the P1 lag
  fix was about.
- The committed shape deviates from the raw path by 0.6–1.3 units on the
  wide pencil (corner rounding) versus < 0.1 for EMA; on a 45-unit tip that
  is invisible, on a 0.5-unit monoline it is 1 unit.
- Cost: 2× the input-stage CPU (still ≤ 0.3 ms per stroke in a debug
  build) and 10–20 % more stored points.

Call: EMA ships. ISM stays behind the `ism` feature (built into the iOS lab
for side-by-side feel, not selectable on the canvas). Revisit only if slow
calligraphy wobble becomes a complaint; the cheap next step then is ISM's
wobble smoother alone in front of the EMA, not the spring model, and the
full model only with its predictor exposed upstream.
