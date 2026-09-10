# Plan: shape recognizer (draw-and-hold → Line / Arrow / Rect / Ellipse)

Status: implemented (Phases A–E; per-phase notes at the end). Draw-and-hold on
the iPad snaps a stroke to a Line / Arrow / Rect / Ellipse stored as a typed
element; the desktop renders the same elements.

## Context

The iPad now owns pen input (`BrushModeler` in core, Metal `InkRenderer`) and
the desktop renders the same lyon meshes. Next step toward Excalidraw-style
diagrams: draw a rough shape, hold the Pencil still, and the ink snaps to a
perfect shape stored as a typed element, so later work (move/resize, arrows
bound to boxes) has a real model to act on. Recognition is deterministic
geometry in core (no ML), identical on both platforms.

Decisions already taken with the user:
- Trigger is draw-and-hold (~500 ms still), Notability/Goodnotes style. No
  always-on recognition; moving again cancels the snap.
- v1 shapes: Line, Arrow, Rectangle (square), Ellipse (circle). Anything
  else stays freehand.
- Shapes are a new typed element, not strokes.

One deviation from the option label the user picked ("new Shape element
container"): shapes live in the **same** per-sketch z-ordered list as
strokes with an `elem` discriminator, not a sibling `shapes` list. A sibling
list loses stroke/shape z-order (rect, then a stroke inside it, then an arrow
over both is the normal case) and the CRDT gives no tie-break for concurrent
inserts; one `LoroMovableList` gives merged insertion order for free, and
`remove_stroke`, the desktop diff and the iOS `order` keep working. The app is
pre-release, so the tolerant reader is the only compatibility work needed.

## Architecture

```
Pencil → PenGestureRecognizer → SketchModel
   penMoved: modeler.push; arm/reset hold timer (still ≤ 3 pt)
   hold fires: recognize(modeler.points()) via FFI → preview snapped outline as local mesh, haptic
   penEnded:  snapped? finishShape(ShapeElement under the wet id) : finishStroke(...)
core: shape.rs (recognizer, Shape::outline) · element.rs (Element, ShapeElement, Binding) · note.rs (elements list)
render: Element::outline() → stroke_mesh (unchanged) → InkRenderer batch / bevy Mesh2d / SVG
```

## Phase A: recognizer in core (`crates/pendant-core/src/shape.rs`, new)

Reuse: `geom.rs::dedupe` (make `pub(crate)`), lift the point-to-segment
distance from `geom.rs::hits` into `pub(crate) fn segment_distance2`,
`StrokePoint`/`PointSize` from `stroke.rs`, test conventions from
`brush.rs` (`run` helper, `proptest!` with an `arb_*` strategy).

API:
```rust
pub enum Shape {
    Line { a: [f32; 2], b: [f32; 2] },
    Arrow { a: [f32; 2], b: [f32; 2] },              // head at b
    Rect { center: [f32; 2], size: [f32; 2], angle: f32 },   // angle 0 = axis-aligned
    Ellipse { center: [f32; 2], radii: [f32; 2], angle: f32 },
}
pub struct Recognition { pub shape: Shape, pub confidence: f32 }
pub struct RecognizerParams { /* thresholds below, Default */ }
pub fn recognize(points: &[StrokePoint]) -> Option<Recognition>;
pub fn recognize_with(points: &[StrokePoint], params: &RecognizerParams) -> Option<Recognition>;
impl Shape {
    pub fn outline(&self, base_width: f32) -> Vec<StrokePoint>;  // force 1, size None, tilt None
    pub fn bounds(&self) -> ([f32; 2], [f32; 2]);
}
```
Square/circle are derived (`size[0] == size[1]`), not variants. Outline:
Line 2 pts; Arrow `a, b, wing1, b, wing2` (lyon round join covers the
retrace; `hairpin_keeps_ink_at_the_turn` already proves it); Rect 5 pts
(first repeated); Ellipse segment count scales with perimeter (≈ every 4 pt,
clamped 24..=256) so zoom does not show facets. All outline points are
`PolylineSample` with `t_ms` spread over the drawn duration.

Pipeline (thresholds relative to the bbox diagonal `diag` or path length
`len`, all in `RecognizerParams`):
1. **Trim hold tail**: drop the suffix confined to `hold_radius = 3 pt` of the
   last point when it lasted ≥ `hold_min_ms = 120`; replace with its
   centroid. Same at the head with radius 1.5 (pen-down blob). Never below 2
   points.
2. `dedupe`; bbox; `diag < 20 pt` → `None`.
3. **Arc-length resample** to `N = clamp(len / (diag/40) + 1, 16, 256)`.
4. **Corners: ShortStraw + turn gate**. `straw[i] = |r[i-3] r[i+3]|`,
   threshold `0.95 × median`; corner = local minimum below threshold **and**
   turn angle ≥ 35° (the gate is what keeps circles corner-free). Endpoints
   are corners. Post-process to a fixed point (≤ 8 rounds): split a segment
   whose path/chord > 1.05 at its min-straw point if it passes the gate,
   else mark it `Curved`; merge corners whose neighbours are collinear
   (path/chord ≤ 1.05) or within 2 spacings.
5. **Closure**: `|start-end| ≤ 0.15·diag`, or ≤ 0.30·diag with the end on the
   first side / start on the last side (overshoot). If closed: cut the
   overshoot (nearest point to the start in the last 25%), treat corners as a
   cycle so a start mid-side merges away and a start on a corner collapses.
6. **Classify** (interior corners `n`):
   open, `n = 0`, all straight → Line; open, `n ≥ 1`, first segment straight
   → Arrow; closed, 4 cycle corners → Rect (5 with one turn < 50° → drop it,
   retry); closed, 0 corners → Ellipse; else `None` (triangles, polygons,
   letters, arcs stay freehand in v1).
7. **Fits**:
   - Line: PCA direction (`θ = ½·atan2(2sxy, sxx−syy)`), mean ⟂ residual ≤
     3% of `len`, `len/chord ≤ 1.05`; `a`,`b` = first/last point projected.
   - Ellipse: PCA angle, extents in that frame → center/radii; accept if mean
     `|ρ−1| ≤ 0.10` (`ρ = sqrt((u/a)²+(v/b)²)`), `|Σturn| ≈ 2π ± 0.5`, no
     straight run > 40% of `len`. Axes within 10% → circle, angle 0. No
     algebraic (Fitzgibbon) fit: needs a 6×6 eigen-solve for no gain.
   - Rect: sides ≥ 0.15·diag, corner angles 90° ± 20°, opposite sides
     parallel ± 15° and length ratio ≥ 0.75; orientation `θ = ¼·atan2(Σℓ
     sin4α, Σℓ cos4α)`; `|θ mod 90°| ≤ 8°` → 0; size = extents in that
     frame; mean distance to nearest side ≤ 5% of `diag`; sides within 10% →
     square.
   - Arrow: tip = first interior corner, shaft passes the Line test; legs =
     later corners + end, each 10–35% of the shaft and 20–70° off the
     reversed shaft, exactly two, on opposite sides. Accepts `A B L1 B L2`,
     `A B L1 L2`, and head-first (run reversed, keep the better). Outline
     wings = clamp(0.22·shaft, 8..40 pt) at ±30°.
8. **Confidence** = `1 − max(value/threshold)` over the tests of the accepted
   shape; `< 0.25` → `None`. Order Line → Arrow → Rect → Ellipse; Rect wins
   ties against Ellipse.

Robustness: direction-agnostic by construction (tests assert reversed
input gives the same shape with a/b swapped); non-finite input filtered by
`dedupe`; any non-finite output field → `None`; no slice indexing on
unproven lengths (`split_first`, `windows`, `get`); `// ast-grep-ignore:
no-as-cast` only where float→int has no trait, per `brush.rs`.

Tests (in `shape.rs` `mod tests` + one integration test):
- Synthetic generators (deterministic LCG): line/rect/ellipse/arrow with
  jitter, rotation, reverse, start fraction on the perimeter, overshoot, and
  an appended 500 ms hold with 2 pt jitter. Feed through
  `BrushModeler::new(Tool::Pen, 4.0)` so the real streamline/min_distance
  path is exercised.
- Positives: axis rect → angle 0; 5° rect snaps; 20° rect stays within 3°;
  square; 100×60 ellipse axes within 5%; circle; line end within 3 pt; both
  arrow patterns + head-first; rect started mid-side; 15% overshoot; all
  reversed.
- Negatives: S, M, Z, C, semicircle, scribble, triangle, figure-8, 8 pt
  tick, single point, all-NaN → `None`.
- Stage tests: hold trim keeps a fast corner; resample spacing uniform;
  ShortStraw finds exactly 4 corners on a square and 0 on a circle.
- proptest: never panics; confidence in 0..=1; outline finite with the
  variant's length; rect/ellipse outline inside input bbox inflated 10%;
  translation/scale equivariance; determinism.
- Corpus: `crates/pendant-core/tests/corpus/shapes/<name>.txt`, header
  `# expect: rect|ellipse|line|arrow|none`, `# tool: pen size: 4`, then `x y
  force t_ms` raw samples; `tests/shape_corpus.rs` replays each through
  `BrushModeler` and asserts the variant. Recorder: `-recordStrokes 1` launch
  arg makes `SketchModel.penMoved` append raw samples to a file in
  Documents (visible in Files); files are named and copied in by hand.
- `cargo mutants -p pendant-core -f crates/pendant-core/src/shape.rs` after
  tests (install cargo-mutants first; not present on this machine).

## Phase B: element model + storage (`crates/pendant-core`)

- `ids.rs`: `ulid_id!(ElementId)`; `pub type StrokeId = ElementId` (one id
  space; wet-ink `stroke:` fields keep working for shapes).
- New `element.rs`: `Style { tool, color, width }`, `Binding { element:
  ElementId, fixed_point: [f32; 2], gap: f32 }` (Excalidraw fixedPoint; never
  set in v1, always parsed), `ShapeElement { id, shape, style, start, end,
  created_ms }`, `enum Element { Stroke(Stroke), Shape(ShapeElement) }` with
  `id()`, `tool()`, `color()`, `base_width()`, `outline()` (= `flatten()` or
  `Shape::outline(base_width)`).
- `note.rs`: keep the `"strokes"` list as the z-ordered element list.
  `add_stroke` also writes `elem: "stroke"`. New `add_shape` writes `id,
  elem: "shape", tool, color, width, created, shape: line|arrow|rect|ellipse`
  plus flat f64 keys (`ax ay bx by` / `cx cy w h angle` / `cx cy rx ry
  angle`) and optional nested `start`/`end` maps `{element, fx, fy, gap}`.
  Flat keys so a later `update_shape` merges per key. Replace `read_stroke`
  with tolerant `read_element -> Result<Option<Element>>` (unknown `elem`
  or malformed entry → warn + skip; binding whose target is absent → None).
  `elements(sketch)` new; `strokes(sketch)` = elements filtered (keeps
  `convergence.rs`, `replay.rs` compiling); `remove_stroke` →
  `remove_element` (body already id-based) with the old name kept as alias.
- `export.rs`: `elements_to_svg` over `el.outline()` (existing `M/L` emitter,
  width = base_width for shapes since force is 1). `NoteDoc::export` uses
  `elements`.
- Tests: shape round-trips through export/import; interleaved
  stroke/shape/stroke keeps order on a replica; `elem: "hologram"` entry is
  skipped; `convergence.rs` gains `Op::AddShape` and asserts `elements()`.

Load-bearing coupling: `crates/pendant-ffi/src/net.rs` `sketch_counts`
(~l.551) uses `strokes().len()` to decide whether to fire `strokes_changed`;
switch it to `elements().len()` in the same commit or remote shapes never
notify the iPad.

## Phase C: FFI (`crates/pendant-ffi`)

- `types.rs`: `Point2 {x,y}` Record; `Shape` uniffi Enum with named fields
  (precedent: `SyncState::Connected { url }`); `Binding`, `ShapeElement`
  Records; `Element` Enum `Stroke(Stroke) | Shape(ShapeElement)`; `From`
  both ways.
- `engine.rs` `NoteSession`: `elements(sketch) -> Vec<Element>`;
  `finish_shape(sketch, ShapeElement)` = commit `add_shape` then wet `End`
  (mirror of `finish_stroke`); `erase_at` iterates elements and hit-tests
  `hits(&el.outline(), el.base_width(), …)`; `remove_element`
  (`remove_stroke` alias kept for now). `strokes()` stays until
  `SketchPreview` migrates.
- `brush.rs`: `recognize_shape(points: Vec<StrokePoint>) ->
  Option<Recognition>` and `element_mesh(element, tolerance) -> IndexedMesh`
  (= `stroke_mesh(el.tool(), &el.outline(), width, tol)`; the outline never
  crosses FFI twice). Also `shape_outline_mesh(shape, tool, base_width,
  tolerance)` for the hold preview.
- Wet ink for a snapped shape: keep streaming raw wet points until pen-up,
  commit the shape **under the wet id**, send `End`. Both receivers already
  swap wet→committed by id (`sketch.rs` ~l.241, `SketchScreen.swift`
  `refreshFromCrdt`), so the remote sees the wobble snap at the same moment
  with no blink; Cancel + fresh id would race across the two channels.
- `tests/engine.rs`: clone the stroke sync test for a shape; assert B sees
  `Element::Shape` with the same id and that `strokes_changed` fired (this
  catches the `sketch_counts` coupling). Regenerate Swift bindings
  (`scripts/build-ios-core.sh`), extend `scripts/smoke/main.swift` with a
  recognize + finishShape round trip.

## Phase D: iPad hold-to-snap (`ios/Pendant/Sources`)

- `SketchScreen.swift` `SketchModel`:
  - `LiveStroke` gains `snap: Recognition?` and `holdTask: Task?`.
  - `penMoved`: after `push`, if the newest coalesced sample moved > 3 pt
    from the hold anchor → cancel `holdTask`, clear `snap` (restore normal
    live mesh), re-arm the anchor; else if no task armed, arm a 500 ms task.
  - Hold fires: `recognizeShape(points: modeler.points())`; on `Some`, set
    `snap`, show `shapeOutlineMesh` as the local mesh (`renderer.setLocal`
    with the outline points, same tool/colour/width), haptic:
    `UICanvasFeedbackGenerator.alignmentOccurred(at:)` on iOS 17.5+ (Pencil
    Pro), else `UIImpactFeedbackGenerator(.light)`.
  - `penEnded`: `snap` set → build `ShapeElement(id: wet id, shape, style
    from the live stroke)`, `finishShape`, `renderer.show(.shape(el), z:)`;
    else the existing `finishStroke` path. Cancelled → `cancelStroke` as
    today.
  - `refreshFromCrdt`: `session.elements`, ids from elements,
    `renderer.show(element, z:)`. `eraseLast` → `removeElement`.
  - `-recordStrokes 1` launch arg: append raw samples per stroke to
    Documents for the corpus.
- `InkRenderer.swift`: `Committed.element: Element`, `show(_ element:, z:)`,
  `elementMesh(element:)` in `show` and `retessellateCommitted`; batch reads
  `element.color`. Everything else unchanged.
- `SketchPreview.swift`: thumbnails and the stale check over `elements` +
  `elementMesh`.
- UI test (`SketchUITests`): under `-anyInput 1`, drag a rough rectangle
  path with a trailing 0.7 s hold (`press(forDuration:thenDragTo:…
  thenHoldForDuration:)`), assert the status label shows `shapes=1`
  (add a shape count to the status line next to `strokes=`).

## Phase E: desktop (`crates/pendant/src/sketch.rs`)

`note.strokes` → `note.elements`; bounds fold and `ink_mesh` over
`el.outline()`; scene map keyed by `ElementId`. Wet replacement by id
unchanged. No desktop recognition (no pen input there).

## Not in v1 (reserved by the schema)

Bindings UI (auto-bind arrow ends to nearby shapes, derive endpoints from
the target on move), `update_shape`, move/resize handles, unsnap (needs the
sender to keep the last freehand points), native SVG primitives, triangles
and polygons.

## Order and effort

| Phase | Ships alone? | Est. |
|---|---|---|
| A recognizer + tests | yes (pure core) | 3 d |
| B element model + storage + `sketch_counts` | yes | 0.5 d |
| C FFI + engine test + smoke | needs B | 0.5 d |
| D iPad hold-to-snap + renderer + thumbnails + UI test | needs A–C | 1.5 d |
| E desktop | needs B | 0.5 d |

Minimal set for "iPad snaps and persists, desktop renders it": A, B (incl.
`sketch_counts`), C, D's `SketchModel`/`InkRenderer` parts, E. Thumbnails
and SVG may trail by a commit; nothing else can, because a half-migrated
`strokes()` silently hides shapes from the desktop diff and the change
notification.

## Verification

- Core: `cargo test -p pendant-core` (unit + proptest + corpus), `cargo
  clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`,
  `ast-grep scan -c "$(realpath ~/.claude/skills/rust/sgconfig.yml)"` on new
  files, `cargo mutants -f crates/pendant-core/src/shape.rs`.
- FFI: `cargo test -p pendant-ffi` (new shape sync test),
  `scripts/swift-smoke.sh`, `scripts/build-ios-core.sh`.
- iPad simulator: `xcodegen generate`, build, run `SketchUITests` (new
  rectangle-snap test + existing ones) against a local `pendant-server
  --token demo`.
- Device: build with `DEVELOPMENT_TEAM=YD2FVR5QH2 -allowProvisioningUpdates`,
  `devicectl install/launch --console`; draw rect/ellipse/line/arrow, hold,
  confirm snap + haptic, pen-up persists; reopen shows the shape; desktop
  (or second device) shows the same shape after sync. Record a dozen real
  strokes with `-recordStrokes 1` into the corpus, including two negatives.


## Done

- **A** `crates/pendant-core/src/shape.rs`: pipeline as designed. Two
  deviations found by the synthetic tests: the hold-trim radius is
  `max(3 pt, 4 % of diag)` because the app judges stillness in screen points,
  so at low zoom a hold spans more canvas; and closure with overshoot uses
  "the end lies on the first quarter of the path" (`overshoot = 0.06·diag`)
  instead of a gap bound, which a 15 % overshoot exceeds. `Shape::outline`
  takes no width: outline points carry force 1 and no size, so the caller's
  `base_width` reaches `stroke_mesh` unchanged. The ellipse straight-run test
  is sagitta-based (midpoint distance to the chord) so hand jitter does not
  hide a straight diameter; closed rings with ≤ 2 soft corners (< 60°) may
  still fit an ellipse, which is what keeps 2:1 ovals with pointy ends.
  Corpus harness in `tests/shape_corpus.rs` with two synthetic seed files;
  device recordings (`-recordStrokes 1`) still to be added. `cargo mutants`
  not run (not installed).
- **B** `element.rs`, `ids.rs` (`ElementId`, `StrokeId` alias), `note.rs`
  (`add_shape`, tolerant `read_element`, `elements`, `remove_element`),
  `export.rs` (`elements_to_svg`), `net.rs sketch_counts` over elements.
- **C** FFI records/enums, `NoteSession.elements/finish_shape/remove_element`,
  `erase_at` over element outlines, `recognize_shape`, `element_mesh`,
  `shape_outline_mesh`; relay test syncs a shape and asserts the change
  notification; Swift smoke recognises and commits a rectangle.
- **D** `SketchModel` hold timer (500 ms, 3 screen pt / zoom), preview via
  `setLocalShape`, `UICanvasFeedbackGenerator` haptic, commit under the wet
  id; `InkRenderer` and `SketchPreview` keyed by element; `shapes=` in the
  status line; `-recordStrokes 1` recorder (Documents shared with Files). UI
  test `testHoldSnapsToShape`.
- **E** desktop scene diff over `note.elements()`.

### Tuning after the first device session (2026-09-10)

Device feedback: lines should run from the pen-down point to the held
point; rectangles and circles were too hard to get. A synthetic matrix of
rough closed shapes (rounded corners 4–14 pt, wobble up to 12 %, jitter to
2.5 pt, 12–15 % of the perimeter left open or overshot) showed every
rejection was the closure gate: a hand-drawn loop rarely closes, and a
circle with 8 % missing already has a 0.18·diag gap. Changes:

- `Shape::Line` snaps to the raw pen-down and pen-held points (an arrow's
  tail likewise); the fit only decides whether it is a line.
- `closure` 0.15 → 0.40·diag (a 300° arc is ~0.35, a C or U is ≥ 0.5).
- A gap that spans a corner is closed through the intersection of the two
  end tangents (`missing_corner`), tried only when the straight-chord ring
  fits nothing, so open circles keep their chord.
- `min_confidence` 0.25 → 0: the per-test thresholds gate; confidence is
  informational.
- The hold trim measures the tail from the cloud's centroid (the last point
  can sit at the cloud's edge), and its radius is passed by the caller
  (`recognize_shape(points, hold_radius)`, iPad: 1.5 × 3 pt / zoom) instead
  of being guessed from the stroke size.
- `ellipse_soft_corner` 60° → 70°; ellipses beyond ~2.5:1 read their tips
  as corners and stay freehand (documented limit, proptest bounded).
- `-recordStrokes 1` also prints each stroke to the console, and the corpus
  accepts `# kind: modeled` files, so device strokes can be captured from an
  attached `devicectl --console` and replayed without the brush model.

### Anchoring to the pen (same day)

Every snapped shape now passes through where the pen landed and where it is
held (`Shape::anchored`): a rectangle moves the side each point is nearest
to (both sides when within 8 % of the short side of a corner), an ellipse
keeps its fitted size and angle and slides its center (two points at least a
radius apart pin it via the unit-circle intersection; closer points, the
normal start-and-hold case, anchor their midpoint since a small radial
mismatch would otherwise demand a large slide). Moves beyond 30 % of the
shape's size are refused and the fit stands. An axis-snapped rectangle drawn
a few degrees off therefore keeps its pen corner and absorbs the rotation in
its far sides. Two recognizer rules changed on the way: the ellipse
soft-corner pre-check is gone (the radial and straight-run tests already
separate a pointed ellipse from a D or a lens), and corner candidates within
one straw window merge (a pointed tip could seed two).

### Resize after the snap (same day)

Moving the pen after a snap no longer discards it: the app regenerates the
shape every move from the snapped shape, the point the pen held and the
current pen point (`Shape::resized`, FFI `resize_shape`), so jitter never
accumulates. A line or arrow moves the endpoint under the pen; a rectangle
drags the side (both at a corner) under the pen with the opposite side
fixed, in its own frame; an ellipse scales about the outline point
opposite the pen, so circles stay circles and an oval keeps its aspect.
Pen-up commits the resized shape. Changing an oval's aspect by dragging is
not supported; redraw instead.
