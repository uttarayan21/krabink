# Plan: one ink renderer on every platform (lyon + own input)

Status: proposed. Replaces PencilKit as renderer *and* input device on iPad;
desktop keeps bevy but tessellates through the same core code. Goal: the live
stroke and the committed stroke are the same pixels, on both devices.

## Why

Today iPad ink goes through two renderers. PencilKit draws the wet stroke
(`PKCanvasView`), then at pen-up `SketchScreen.swift` clears the `PKDrawing`
and the Rust ribbon (`geom.rs::ribbon`) fills a `CAShapeLayer`. The ribbon has
butt caps, no joins and no antialiasing control, so the swap is visible. Apps
like Concepts avoid this by owning the whole pipeline: raw touches → own curve
model → own GPU renderer, wet and dry ink identical.

## Target architecture

```
                 ┌───────────── pendant-core ─────────────┐
touches ──►  brush::Modeler  ──►  StrokePoint[]  ──►  tess::mesh  ──► Vec<Vertex>
 (UIKit /        smoothing,        (PolylineSample,      lyon variable
  bevy)          width model       existing schema)      width, round
                                                          caps/joins;
                                                          nib_ribbon for Brush
                 └────────────────────────────────────────┘
iPad:   MTKView draws meshes (MSAA 4x), UIScrollView drives pan/zoom
desktop: bevy Mesh2d from same vertices, camera Msaa::Sample4
```

Everything above the renderer is shared. Only the "upload vertices, draw
triangles" step is per platform.

## Work breakdown

### Phase 0: core tessellation on lyon (Rust only, no UI change)

Files: `crates/pendant-core/src/geom.rs` (split into `geom/flatten.rs`,
`geom/tess.rs`), `Cargo.toml` workspace deps.

1. Add `lyon_tessellation = "1.0"` + `lyon_path` (workspace dep). No `std`
   concerns: core already uses std. Check iOS static lib builds
   (`scripts/build-ios-core.sh`) still link; lyon is pure Rust.
2. New `tess::Mesh { positions: Vec<[f32;2]>, indices: Vec<u32> }` replacing
   `RibbonMesh`. Keep field names so `sketch.rs::ribbon_mesh` and the FFI
   `stroke_triangles` need only a rename.
3. `tess::stroke_mesh(tool, points, base_width, tolerance) -> Mesh`:
   - Build `Path::builder_with_attributes(1)`; attribute 0 = per-point width
     (`size.w` when present, else `base_width * force`, floored by
     `MIN_FORCE` as today).
   - `BSplineControl` strokes: convert each uniform cubic B-spline span to its
     Bézier equivalent and emit `cubic_bezier_to` with width attributes at the
     knots. Lyon flattens by `tolerance`, so zoom-dependent quality is free.
   - `PolylineSample` strokes: `line_to` per sample after `dedupe`.
   - `StrokeOptions::tolerance(t).with_line_width(1.0)
     .with_variable_line_width(0).with_line_cap(LineCap::Round)
     .with_line_join(LineJoin::Round)`. `StrokeVertex::line_width()` already
     returns `line_width × attribute`, so the constructor just reads
     `position()`.
   - Single point → lyon draws a round dot from caps alone; drop the hand
     made quad.
   - `Tool::Brush` keeps `nib_ribbon` (orientation-driven, lyon has no
     concept of a flat nib). Route in `ribbon_for` → rename `mesh_for`.
4. `tolerance` parameter: canvas units. Callers pass `0.25 / zoom` so lines
   stay smooth when zoomed in. FFI default 0.25.
5. Tests (core has `test = true`): mesh non-empty for 1/2/N points; no NaN;
   cap adds vertices beyond endpoints by ≈ half width; B-spline path
   interpolates endpoints (compare against `flatten_stroke` samples within
   tolerance); proptest over random strokes for no panic. Keep
   `flatten_stroke` for the SVG exporter and hit-testing.
6. Update `export.rs` SVG: `ribbon_outline` cannot come from a lyon mesh.
   Either keep the old ribbon outline for SVG (acceptable, export only) or
   emit `<path>` from flattened points with `stroke-linecap="round"` and a
   constant width. Decide: keep old outline for now, mark as debt.

Exit: `cargo nextest run --workspace`, clippy, fmt green. Desktop renders
strokes with round caps/joins (it already goes through `ribbon_for`).

Done (commit after `40278b2`). Decisions taken while implementing:
- B-spline strokes are flattened first (`flatten_stroke`, 8 samples per
  span) and fed to lyon as `line_to` segments, so width interpolates
  linearly along the arc. Lyon's Bézier path is not used; revisit only if
  the sample count shows up in profiles.
- Public API: `StrokeMesh`, `stroke_mesh(tool, points, base_width,
  tolerance)`, `mesh_triangles(..)`, `DEFAULT_TOLERANCE = 0.25`.
  `ribbon_outline` stays (legacy flat ribbon) for the iPad's CAShapeLayers
  and is the only caller of the old ribbon code. FFI `stroke_triangles` /
  `wet_triangles` gained a `tolerance` argument and `default_tolerance()`.
- Non-finite points are dropped before tessellation; a lyon error logs a
  warning and yields an empty mesh instead of panicking.
- Tolerance is clamped to a quarter of the thinnest width in the stroke:
  lyon drops geometry finer than its tolerance, so a coarse tolerance on
  hairline ink erased the whole stroke (found by the proptest).

### Phase 1: brush model in core (input pipeline)

Files: new `crates/pendant-core/src/brush.rs`, `wetink.rs`, FFI `types.rs`.

1. `brush::Params` per `Tool`: `size`, `thinning` (speed → width), `smoothing`
   (streamline EMA factor), `taper_start/end`, `min_force`. Constants live in
   core so both platforms agree. Pen: thinning 0.5, streamline 0.5. Marker:
   thinning 0, opaque wide. Monoline: thinning 0, streamline 0.2. Brush: nib.
2. `brush::Modeler` (stateful, one per live stroke):
   - `push(raw: RawSample{x,y,force,t_ms,tilt}) -> Vec<StrokePoint>` applies
     streamline (`p = prev + (raw - prev) * (1 - smoothing)`), computes speed
     from `t_ms`, width = `size * lerp(1, force, thinning) * speed_falloff`,
     stores into `StrokePoint.size = Some(w)`. Emits `PointKind::PolylineSample`.
   - `predict(raw_predicted: &[RawSample]) -> Vec<StrokePoint>` same math on a
     scratch copy, never mutates state. Renderer appends these as a tail and
     discards them next frame (Apple's predicted-touch contract).
   - `finish() -> Vec<StrokePoint>` flushes the EMA lag (emit the last raw
     point) and applies end taper.
3. Optional later: swap the EMA for `ink-stroke-modeler-rs` behind the same
   `Modeler` API. Not in this plan; EMA is deterministic and testable.
4. `WetPoint` already carries `width: Option<f32>` and `nib`. Sender now fills
   `width` from the modeler so receivers draw identical ink. No wire change.
5. Storage: commit `PolylineSample` points after `StrokePoint::quantized()`.
   Existing `BSplineControl` strokes from the PencilKit era still flatten and
   render; nothing migrates. Optional later: B-spline fit at pen-up (Concepts'
   "Curve Fitter") to shrink point data; keep chunk encoding as is.
6. FFI: `#[derive(uniffi::Object)] BrushModeler` with `push`, `predict`,
   `finish`; `mesh_for_stroke(stroke, tolerance) -> Vec<f32>` and
   `mesh_for_points(points, tool, base_width, tolerance) -> Vec<f32>` (flat
   x,y triangle list, same shape `stroke_triangles` returns today so the
   iOS decode helper `InkView.polygon` survives until Phase 2 replaces it).
   Return indexed meshes (`positions`, `indices`) as a uniffi record to halve
   the buffer size for Metal.
7. Tests: modeler determinism (same input → same output), streamline reduces
   jitter (variance of second differences drops), speed thinning monotonic,
   predict never mutates, finish emits last point exactly.

Exit: core + FFI build; `scripts/swift-smoke.sh` passes.

Done (commit after `aa209b0`). Decisions taken while implementing:
- `BrushModeler` owns the live stroke's points: `push(raw) ->
  Option<StrokePoint>` (None when the smoothed point moved less than
  `min_distance`), `points()`, `predict(&[raw])`, `finish()`. The renderer
  draws `points()` + `predict()` each frame; `finish()` is the commit.
- `RawSample.t_ms` is `f64` on any monotonic clock (`UITouch.timestamp *
  1000`); the modeler zeroes it at the first sample, so callers never
  compute stroke-relative times.
- Width = `size * lerp(1, force, thinning) * (1 - thinning * 0.5 *
  clamp(speed / speed_ref)) * lead_in`, floored at `size * min_width`.
  Speed is an EMA over raw samples. The end taper spans `taper_end` or half
  the stroke, whichever is shorter, so dots and ticks keep their width.
- Tuning lives in `BrushParams::for_tool`: pen thinning 0.5 / streamline
  0.5 / tail taper 1.5×size; marker streamline 0.35; monoline 0.2; brush
  constant width (the nib does the shaping). Expect to retune on device.
- FFI: `BrushModeler` object (`push(samples)`, `points`, `predict`,
  `finish`), `RawSample`, `IndexedMesh { positions, indices }`,
  `stroke_mesh(stroke, tolerance)`, `points_mesh(points, tool, base_width,
  tolerance)`, `wet_points(points)` (StrokePoint → WetPoint, keeps width +
  nib). `stroke_triangles` / `wet_triangles` / `*_outline` stay until
  Phase 2 removes their callers.
- Nothing on iOS uses the modeler yet: PencilKit still owns input until
  Phase 2, and the desktop has no pen input. The Swift smoke test drives
  the full push → predict → finish → mesh → commit path instead.

### Phase 2: iPad – own canvas view (the big one)

Files: `ios/Pendant/Sources/SketchScreen.swift` (rewrite), new
`InkRenderer.swift` (Metal), new `Shaders.metal`, `StrokeCodec.swift` (shrink),
`project.yml` (add `.metal` to sources, it is picked up automatically by
XcodeGen when under `Sources`).

1. **View stack**: `SketchCanvasView: UIView` containing
   - `UIScrollView` (finger pan, pinch zoom, bounce, deceleration) with an
     empty content view sized to `contentSize`. Same `grow()` logic as today.
   - `MTKView` pinned to the screen, *not* inside the scroll view. Each frame
     reads `contentOffset` / `zoomScale` into a uniform view matrix. Avoids a
     16k-texture ceiling and keeps memory flat at any zoom.
   - `drawingPolicy` equivalent: `UITouch.type == .pencil` draws; direct
     touches go to the scroll view (`.pencilOnly`), with a debug toggle for
     simulator fingers (`.anyInput`) so the UI tests keep working.
2. **Input**: override `touchesBegan/Moved/Ended/Cancelled` on the canvas
   view (replaces `ActiveObserverGestureRecognizer`).
   - `Moved`: `event.coalescedTouches(for:)` → `modeler.push` each;
     `event.predictedTouches(for:)` → `modeler.predict` → renderer tail.
   - Convert with `location(in: contentView)` so coordinates stay canvas
     space regardless of zoom.
   - `touch.estimatedPropertiesExpectingUpdates` + `touchesEstimatedPropertiesUpdated`:
     Pencil delivers force/azimuth late for the first samples; patch the
     modeler's stored points by `estimationUpdateIndex`. Skip for v1, note
     as follow-up (PencilKit did this for us).
   - Palm rejection is free: only `.pencil` touches ink.
3. **Renderer** `InkRenderer` (Metal, `MTLDevice`, one pipeline, MSAA 4x
   via `MTKView.sampleCount = 4`):
   - Vertex: `float2 position`; uniforms: view matrix, `float4 color`.
   - Committed strokes: one `MTLBuffer` pair (positions, indices) per stroke,
     cached by stroke id, rebuilt when `strokesChanged` diffs the CRDT or
     when zoom crosses a tolerance bucket (`tolerance = 0.25 / zoom`, bucket
     by power of two so zooming does not thrash).
   - Wet strokes (local + remote): a growable ring buffer per stroke; on new
     points re-tessellate only the last N points (lyon has no incremental
     mode; tessellate the tail window of ~16 points and stitch, or simply
     re-tessellate the whole wet stroke each frame. Measure first: a 2000
     point stroke through lyon is well under a millisecond, so whole-stroke
     is likely fine).
   - Predicted tail: separate tiny buffer, drawn last, replaced each frame.
   - Draw order: committed by CRDT z, then remote wet, then local wet, then
     prediction. Marker uses `alpha < 1` blending; others opaque.
   - `isPaused = true`, `enableSetNeedsDisplay = true`; call
     `setNeedsDisplay()` on input, wet ink, CRDT change, scroll/zoom. Idle
     canvas costs no GPU.
   - Background: `systemBackground`, flip y in the view matrix (core is
     y-down, Metal NDC is y-up).
4. **Tools**: keep `PKToolPicker`. It works with any first responder
   (`setVisible(true, forFirstResponder: canvasView)`), and
   `PKToolPickerObserver.toolPickerSelectedToolDidChange` hands us
   `PKInkingTool`/`PKEraserTool`. Map ink type → `Tool` via the existing
   `StrokeCodec.tool`, width/color → brush params. Lasso and ruler are
   dropped for v1 (ruler could return as a snap-to-line modeler later).
5. **Eraser**: unchanged. `erase_at` in core; sample eraser touches the same
   way, radius from the picker's `PKEraserTool.width`.
6. **Undo**: `eraseLast` stays. Later: wire `UIPencilInteraction` double-tap
   to eraser toggle and squeeze to the picker (iOS 17.5).
7. **Commit path**: pen-up → `modeler.finish()` → `Stroke{kind: PolylineSample,
   points, ...}` → `finish_stroke` (same id as the wet stroke, so receivers
   swap wet for committed with identical geometry). `StrokeCodec.encode/
   decode` PKStroke paths and `renderedWidthScale` go away.
8. **SketchPreview.swift**: inline previews currently draw via PencilKit
   import + CoreGraphics fill of `stroke_triangles`. Switch to the same
   `mesh_for_stroke` and keep the CoreGraphics fill (previews are static
   thumbnails; no need for Metal there).
9. **UI tests**: keep `accessibilityIdentifier = "sketchCanvas"` on the
   canvas view; `SketchUITests` drag gestures then still ink under
   `.anyInput`. Add a launch argument `--any-input` the tests pass.

Exit: draw on device, no visible change at pen-up. Compare a screenshot
before and after pen-up for the same stroke (pixel diff ≈ 0 apart from the
predicted tail).

Done (commit after `2ec5786`). Decisions taken while implementing:
- Files: `SketchScreen.swift` rewritten (`SketchModel`, `PenGestureRecognizer`,
  `SketchCanvasView`, `SketchCanvas`), new `InkRenderer.swift` (Metal,
  `GPUMesh`, `Viewport`, `IndexedMesh.cgPath/bounds`), new `Shaders.metal`,
  `StrokeCodec.swift` down to tool/colour mapping, `SketchPreview.swift`
  thumbnails from `strokeMesh`.
- Input goes through a `UIGestureRecognizer` on the scroll view (not
  `touchesBegan` on the view): it is the proven path from the PencilKit
  era, receives the `UIEvent` for coalesced/predicted touches, and
  coexists with the scroll view's pan/pinch. Locations are read in the
  zoomable content view so they are canvas coordinates at any zoom.
- `InputPolicy`: pencil-only on device (pan/pinch take `.direct`
  touches only), any input on the simulator; `-anyInput 0|1` overrides.
  Under any-input the scroll pan needs two fingers so one finger inks; a
  second touch during a stroke cancels it (a pinch, not ink).
- Pencil force: `min(touch.force, 1)`, i.e. average pressure = full
  width; fingers report 1. Retune with the brush params on device.
- Renderer: one pipeline, straight-alpha blending, `MTKView.sampleCount
  = 4`, demand-driven (`isPaused`, `enableSetNeedsDisplay`). Committed
  meshes are cached per stroke and rebuilt when the zoom crosses a
  power-of-two bucket; wet and live strokes are re-tessellated whole on
  every update. The live stroke is `modeler.points() + predict(tail)`.
- Remote wet ink uses the new FFI `wet_mesh` (WetPoint run → indexed
  mesh), so receivers draw the sender's widths exactly.
- `PKToolPicker` stays attached to the canvas view as first responder;
  iOS 18's `selectedToolItem` and iOS 17's `selectedTool` both feed
  `SketchModel.tool`. Lasso and ruler are gone.
- Thumbnails fill the mesh triangles with antialiasing off at 2× and let
  the frame's downscale smooth the edges; antialiased adjacent triangles
  leave hairline seams.
- Estimated touch property updates are still ignored (follow-up).
- `stroke_triangles` / `wet_triangles` / `stroke_outline` / `wet_outline`
  and core `ribbon_outline` have no callers left; Phase 4 removes them.

### Phase 3: desktop parity

Files: `crates/pendant/src/sketch.rs`.

1. `ribbon_mesh` → `stroke_mesh` (Phase 0 rename), tolerance from the
   sketch's render scale.
2. Offscreen camera: add `Msaa::Sample4` to the camera entity so edges
   match the iPad's MSAA.
3. Wet ink: same as today but through the new mesh; latency telemetry
   unchanged.
4. Follow-up, not this plan: desktop pen input (bevy `PointerInput`
   pressure via winit tablet events) through the same `brush::Modeler`. All
   the pieces will exist; only a system that feeds mouse/tablet events is
   missing.

### Phase 4: cleanup

- Delete `ActiveObserverGestureRecognizer`, `PKStroke` codec paths,
  `SpikeScreen` PencilKit bits if unused, `renderedWidthScale`.
- `PointKind::BSplineControl` stays for old data; document that new
  strokes are always `PolylineSample`.
- Update `docs/architecture.md` ink section and the header comment in
  `SketchScreen.swift`.

## Risks and calls

- **Look changes vs PencilKit.** Unavoidable by design. Users lose Apple's
  pencil texture and marker chisel. Gain: identical ink everywhere. Tune
  `brush::Params` against Apple's pen on a side-by-side before merging.
- **Latency.** PencilKit is ~1 frame ahead thanks to prediction and Metal.
  Ours: coalesced + predicted touches + `MTKView` at 120 Hz should land in
  the same range. Measure with the existing `WetLatency` telemetry pattern
  plus a local pen-down-to-present timer; target < 20 ms on M-series iPad.
- **Lyon variable width interpolates by Bézier `t`, not arc length.** For
  B-spline strokes with long spans width can drift unevenly. Mitigation:
  emit shorter spans (split each span in two) or flatten first and use
  `line_to` (width then interpolates linearly per segment). Decide after
  seeing real strokes; default to flatten-then-line_to if in doubt.
- **Tessellation cost at zoom.** Tolerance buckets + per-stroke caching keep
  rebuilds rare. Worst case (zoom in, thousands of strokes) is bounded by
  rebuild-on-bucket-change; profile with a 5k-stroke sketch.
- **Nib tool stays custom.** `nib_ribbon` has no caps either; add round end
  caps by appending lyon-style arc fans, or accept flat nib ends (they are
  correct for a calligraphy nib).
- **Estimated touch properties.** First few samples of a stroke can have
  placeholder force. v1 ignores updates; strokes may start slightly thin.

## Order and effort

| Phase | Depends on | Size |
|---|---|---|
| 0 lyon in core | – | 1–2 days |
| 1 brush model + FFI | 0 | 1–2 days |
| 2 iPad canvas + Metal | 1 | 5–8 days |
| 3 desktop parity | 0 | 0.5 day |
| 4 cleanup + docs | 2, 3 | 0.5 day |

Phases 0 and 3 ship on their own and already improve desktop and remote
ink. Phase 2 is the only user-visible break on iPad and should land as one PR
behind a build flag until latency is measured.

## Out of scope

Desktop pen input, lasso/selection, ruler, textured brushes, B-spline curve
fitting at pen-up, ink-stroke-modeler integration. Each is a follow-up that
plugs into `brush::Modeler` or `tess::stroke_mesh` without touching the wire
format.
