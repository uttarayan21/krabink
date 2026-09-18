# Pendant: stack and implementation

State of the tree on branch `research-brush-strokes` at `34e7ab5`
(2026-09-18). This is the map: what the stack is, where each piece lives,
and how the pieces talk to each other. The design rationale lives in the
plans (`docs/plans/*.md`, each with per-phase implementation notes) and the
sync topology in `docs/architecture.md`; this document points at them
rather than repeating them.

## 1. Stack at a glance

| Layer | Technology | Where |
|---|---|---|
| Language, toolchain | Rust, edition 2024, workspace resolver 3; nix flake (crane) for CI, dev shells and packages | `Cargo.toml`, `flake.nix` |
| Document model | Loro 1.13 CRDT wrapped so no Loro type escapes; postcard for wire and chunk encoding; ULID ids | `crates/pendant-core/src/{note,workspace,sync_doc,ids,stroke}.rs` |
| Persistence | redb 4 (`snapshots` + `updates` tables keyed by `DocKey(u128)`) | `crates/pendant-core/src/store.rs` |
| Sync protocol | sans-io state machines, one version byte + postcard frame, WebSocket transport | `crates/pendant-core/src/sync.rs` |
| Ink | `BrushSpec` presets, EMA input model, tip evaluator, lyon 1.0 stroker and a convex-hull nib sweeper, one `InkMesh` type | `crates/pendant-core/src/{brush,geom}/` |
| Shapes | deterministic draw-and-hold recogniser (ShortStraw corners, PCA fits) | `crates/pendant-core/src/shape.rs` |
| Relay | axum 0.8 with `ws`, tokio, bearer token, redb behind a `DocProvider` | `crates/pendant-server` |
| Desktop | Bevy 0.19.1 (`wayland`), bevy_egui 0.42, egui_commonmark 0.25, custom `Material2d` + WGSL; embedded relay with mDNS (`_pendant._tcp`) | `crates/pendant` |
| iPad bridge | UniFFI 0.32 proc-macro bindings, staticlib per iOS target, XCFramework + generated `Pendant.swift` in a local SPM package | `crates/pendant-ffi`, `ios/PendantCore` |
| iPad app | SwiftUI + UIKit, iOS 17 deployment target, Metal renderer (4x MSAA, sRGB, depth), PencilKit only as the tool picker, VisionKit QR scanner, Bonjour discovery; XcodeGen project | `ios/Pendant` |

## 2. Repository layout

```
Cargo.toml              virtual workspace: crates/*
flake.nix               crane checks (clippy, fmt, toml-fmt, audit, deny, nextest, llvm-cov, docs), packages, dev shells
crates/pendant-core     platform-free core (no async, no UI); ~8.4k lines
crates/pendant-server   relay library + `pendant-server` binary
crates/pendant          desktop binary (Bevy) with embedded relay, `pair` and `replay` subcommands
crates/pendant-ffi      UniFFI surface for Swift, `uniffi-bindgen` bin behind the `bindgen` feature
ios/PendantCore         SPM package: generated XCFramework + Pendant.swift (outputs of scripts/build-ios-core.sh)
ios/Pendant             XcodeGen spec (project.yml), Sources/, UITests/
scripts/build-ios-core.sh   cargo rustc for aarch64-apple-ios{,-sim} → bindgen → xcodebuild -create-xcframework
scripts/swift-smoke.sh      host cdylib + bindgen + scripts/smoke/main.swift, run on macOS
docs/architecture.md    sync topology, pairing, bridging, failure modes
docs/plans/             ink-renderer (done), shape-recognizer (done), brush-engine (P0–P2 done, P3–P4 planned)
.github/workflows       build.yaml (nix check matrix + llvm-cov → codecov), docs.yaml (cargo doc check)
```

## 3. Core crate (`pendant-core`)

Compiles unchanged for Linux, macOS and iOS. Everything platform-specific
sits above it.

### 3.1 Documents

- `NoteDoc` (`note.rs`): one Loro doc per note. Containers: `meta` (title),
  `text` (markdown), `sketches`, and per sketch an element list stored
  under the key `strokes` where each entry is tagged `elem = stroke | shape`.
  API is text splice, title, create sketch, add/remove stroke or shape,
  `elements()` in z order, and the CRDT triad `version`,
  `export_updates_since`, `import_update`, `export_snapshot`.
- `WorkspaceDoc` (`workspace.rs`): registry of notes (`NoteMeta`) and paired
  devices (`DeviceMeta`) so the library UI never opens every note.
- `SyncDoc` (`sync_doc.rs`): semantics-free import/export/version; what the
  relay holds.
- `Store` (`store.rs`): snapshot plus append-only update log per `DocKey`,
  with `Flush` control and `checkpoint`. Shared by server, desktop and iPad.
- Ids (`ids.rs`): `NoteId`, `SketchId`, `StrokeId`, `ElementId`, `DeviceId`,
  all ULIDs; `seed()` yields the low 32 bits for stroke-mapped grain.
- `export.rs`: markdown export rewriting `pendant://sketch/<id>` embeds to
  SVG assets rendered from the elements through `geom/outline.rs`.
- `pair.rs`: `PairInfo` ⇄ `pendant://pair?server=…&token=…[&alt=…][&fallback=…][&relay=…]`.

### 3.2 Sync protocol (`sync.rs`)

`ClientMsg` (`Hello`, `ListDocs`, `Subscribe`, `Unsubscribe`, `Update`,
`Ephemeral`) and `ServerMsg` (`HelloAck`, `DocList`, `SubscribeAck`,
`Update`, `Ephemeral`, `Error`). `ClientSession` and `ServerSession` are
pure: feed a decoded frame, get back `ClientEffect`s or `ServerEffect`s
(`Send`, `Broadcast`, `Disconnect`, `DocSynced`, …). Docs are reached through
the `DocProvider` (server) and `ClientDocs` (client) traits, so the same
code drives the desktop, the relay and the FFI. `PROTO_VERSION` is 1.
Ephemeral payloads are opaque to the protocol; wet ink is one of them.

### 3.3 Strokes and elements

- `Stroke` (`stroke.rs`): `id`, `tool`, optional `CustomBrush`, `color`
  (`Rgba`, packed as i64 in the CRDT), `base_width`, `kind`, `points`,
  `created_ms`. `Stroke::spec()` resolves the preset or the custom spec.
- `StrokePoint`: `x, y, force, t_ms, tilt: Option<Tilt{azimuth, altitude,
  roll}>, size: Option<PointSize>`. Points are quantised and chunk-coded
  with postcard (`encode_chunks`/`decode_chunks`), the same encoding on the
  wire and in the doc.
- `Tool`: `Pen`, `Pencil`, `Marker`, `Monoline`, `Fountain`.
- `Element` (`element.rs`): `Stroke(Stroke)` or `Shape(ShapeElement)`.
  `ShapeElement` holds a `Shape` (line, arrow, rectangle, ellipse), a
  `Style` and reserved Excalidraw-style `Binding`s.
- `WetInk` (`wetink.rs`): `Begin{sketch, stroke, tool, color, base_width,
  spec}` → `Points{seq, sent_ms, chunks}`* → `End{tail}` or `Cancel`. The
  points are the modelled stage-1 points, so a receiver runs the identical
  fold and the commit replaces the provisional ink without a visible change.

### 3.4 Ink pipeline (`brush/`, `geom/`)

Three pure stages. Every consumer (live tail, remote wet copy, committed
stroke, thumbnail, SVG) runs the same fold.

1. **Input** (`brush/input.rs`): `RawSample{x, y, force, t_ms, tilt,
   estimate}` → `EmaModel` (positional streamline, speed EMA, min distance,
   min force) → `StrokePoint`. `BrushModeler` (`brush/mod.rs`) wraps it with
   `push`, `predict`, `finish`, `pending_estimates` and `update(id, force,
   tilt)`, which patches a stored point and re-runs the fold from it when
   the Pencil delivers an estimated property late.
2. **Dynamics** (`brush/dynamics.rs`): `TipEvaluator` folds each point into
   a `TipState` (width, height, opacity, rotation) by applying the spec's
   `Behavior`s: `Source` (`Pressure`, `Speed`, `Tilt`, `DistanceFromStart`,
   `DistanceToEnd`) through a `Curve` into a `Target` (`Width`, `Height`,
   `Size`, `Opacity`, `Rotation`) with per-behaviour damping and a size rate
   limit. `StrokeEnd` says whether the stroke is still live or complete.
3. **Geometry** (`geom/`): `Ink` (spec + colour + base width + seed) turns
   tip states into an `InkMesh`. Round tips go through lyon's variable-width
   stroker with round caps and joins (`continuous.rs`); oriented nibs are
   the convex hull of the nib rectangle at consecutive points
   (`nib.rs`), so no corner sticks out. Points closer than a fifth of the
   base width are merged first. Stamped brushes (`stamps.rs`) lay one quad
   per dab along the arc length with tip-space uv, jittered by `brush/rng.rs`
   (a hash of seed, dab index and channel, so live and committed dabs are
   identical); such meshes are `zoom_independent`. `outline.rs` produces
   one closed polygon for SVG. `Ink::hover_dab` is the one-point mesh for
   the Pencil hover preview.

`BrushSpec` (`brush/spec.rs`): `input: InputParams`, `tip: Tip{aspect,
corner, orient: Motion | Nib{fallback} | Fixed, hardness, max_size_rate,
min_size}`, `dynamics: Vec<Behavior>`, `emit: Continuous |
Stamped{spacing, scatter, rotation_jitter, size_jitter, opacity_jitter}`,
`paint: Paint{opacity, overlap: Accumulate | Discard, blend: Normal |
Multiply, grain: Option<Grain{source: Noise | Image(AssetId), mapping:
Canvas | Stroke, scale, strength}>}`; `tip.mask` is `Shape` or
`Image(AssetId)`. Specs serialise with `SPEC_VERSION` 4 for custom
brushes carried inside a stroke, a wet `Begin`, or the workspace's
`brushes` map (`BrushMeta`). `BrushSpec::builtins()` bundles
`builtin:crayon`, `builtin:pencil-grainy` and `builtin:chalk`;
`BrushSpec::builtin_assets()` the paper grain and chalk mask PNGs
(`AssetId` = content hash, `Asset::from_png` validates ≤ 64 KiB);
`BrushKnobs` is the editor's view of a spec (`knobs()` / `with_knobs()`).

Presets (`BrushSpec::preset`):

| Tool | Tip | Dynamics | Paint |
|---|---|---|---|
| Pen | round, hard | pressure and speed drive size, taper towards the end | opaque, accumulate |
| Pencil | round, hardness 0.7 | tilt widens and lightens, pressure drives opacity and size | 0.9, **discard**, canvas noise grain 1.5 / 0.55 |
| Marker | chisel, aspect 0.25, nib orientation, fallback +45° without tilt | constant | 0.45, **discard**, **multiply** |
| Monoline | round | none | opaque, accumulate |
| Fountain | nib, aspect 0.15, follows azimuth minus roll, fallback −45° | pressure drives width | opaque, accumulate |

`InkMesh` is `vertices: Vec<InkVertex{pos, uv, opacity}>`, `indices`,
`zoom_independent`, and one `InkStyle{color, opacity, blend, overlap,
hardness, mask: Ribbon | Shape{corner} | Image{asset, corner}, grain:
Option<GrainStyle{image, mapping, scale, strength, seed}>}`. On a ribbon `uv.x` is arc length in canvas units
and `uv.y` the side in −1..1; on a dab `uv` is the tip space `[-1, 1]²`;
that is what the shaders map masks and grain through. `DEFAULT_TOLERANCE`
is 0.25.

### 3.5 Shapes (`shape.rs`)

`recognize(points)` (and `recognize_with(params)`) runs on the modelled
points when the pen holds still: hold trimming, arc-length resampling,
ShortStraw corners, closure test, PCA and corner fits, thresholds relative
to the stroke's size. A hit is a `Recognition` with a `Shape` and a
confidence; pen-up commits a `ShapeElement` under the wet stroke's id.

### 3.6 Tests and corpora

- 89 inline unit tests plus proptest properties (geometry, recogniser
  equivariance, CRDT convergence in `tests/convergence.rs`).
- `tests/loopback.rs`: client and server sessions wired back to back with
  malformed-frame fuzzing.
- `tests/golden_mesh.rs`: fixed strokes through the whole pipeline, hashed
  against `tests/golden/mesh.txt`; regenerate with `PENDANT_UPDATE_GOLDEN=1`.
- `corpus.rs` parses recorder v1 (4 columns) and v2 (7/8 columns with tilt
  and an `est` flag) files. `tests/corpus/shapes/` has seven recordings for
  `tests/shape_corpus.rs`; `tests/corpus/brush/` has fifteen iPad
  recordings (fountain, pencil, pen, marker, monoline) for
  `tests/brush_corpus.rs`, which measures every input model the build has
  (`corpus::StrokeMetrics`: jitter, lag, deviation from the raw path,
  overshoot, width range, vertex count, µs), asserts loose bounds and
  dumps rows with `PENDANT_METRICS_JSON=path`.
- `brush/ism.rs` (feature `ism`): the ink-stroke-modeler input model
  behind `InputModelKind::Ism` / `BrushModeler::with_model`; the P4 trial,
  not what the canvas uses (decision in `docs/plans/brush-engine.md`).

## 4. Relay (`pendant-server`)

`AppState::new(store, tokens)` builds the axum router with one route,
`/ws`. Each connection owns a `ServerSession`; `PeerRegistry` fans out
`Broadcast` effects; `ServerDocs` lazily loads `SyncDoc`s over the store,
checkpoints every 30 s and unloads idle ones (`maintenance`). The binary
takes a TOML config or `--listen`, `--db`, `--token` (repeatable). The
library is what the desktop embeds in-process. `tests/relay.rs` mounts the
real router on an ephemeral port.

## 5. Desktop (`pendant`)

Bevy app with egui UI. Modules:

- `config.rs`: data dir (store, device id, `relay_token`) and
  `~/.config/pendant/config.toml` (`server`, `fallback`, `token`), all
  overridable by `cli.rs` flags (`--data-dir`, `--server`, `--token`,
  `--relay-listen`, `--follow-latest`). Subcommands: `completions`, `pair <uri>`,
  `replay` (headless 120 Hz latency rig).
- `docs.rs`: workspace registry and open notes over the shared store.
- `relay.rs`: `EmbeddedRelay` serving the server router on `0.0.0.0:8722`
  (ephemeral port if taken), advertised over mDNS with the device id in
  TXT.
- `sync.rs`: one tokio task per relay link owning its WebSocket with
  backoff; the Bevy side drives one `ClientSession` per link each frame and
  bridges updates between links. Link 0 is the embedded relay, link 1 the
  optional dedicated relay. See `docs/architecture.md`.
- `ui.rs`: library sidebar, markdown editor bound to the CRDT by
  prefix/suffix diffing, live preview. `settings.rs`: sync state, devices,
  pairing QR, paste-to-join.
- `sketch.rs`: each sketch is an off-screen Bevy scene (own render layer and
  camera) rendered into an `Image` and handed to egui through a
  `pendant://` texture loader. Committed elements are ink meshes at z
  `k/100`; remote wet strokes at `990 + j/100` are dropped when the commit
  lands or on timeout.
- `lab.rs`: `pendant brush-lab --corpus <file|dir> --presets … --models
  ema,ism --out dir --svg --metrics`, the tuning bench: SVG grids per
  recording through `elements_to_svg` and `metrics.json` from
  `corpus::StrokeMetrics`; the `ism` feature adds the ISM row.
- `ink_assets.rs`: `InkAssets` resource, the mask and grain `r8` texture
  arrays (CPU box mips, `image` crate decoding) from the bundled PNGs and
  the workspace's assets, rebuilt when `asset_ids()` changes; scenes
  respawn their strokes when its generation moves.
- `ink_material.rs` + `ink.wgsl`: the desktop twin of the Metal pipeline.
  One `Material2d` per `InkCombo` (blend × overlap, four per sketch)
  sharing one `InkPalette` storage buffer of `StrokeStyle`; each mesh
  carries a `MeshTag` index into it plus the custom `ATTRIBUTE_INK_UV` and
  `ATTRIBUTE_INK_OPACITY` vertex attributes. Reversed-Z: accumulate runs
  compare `GreaterEqual`, discard runs `Greater` with depth write. The
  palette pads to a power of two and re-touches its materials for two
  frames after growth because `PreparedMaterial2d` does not re-prepare on
  buffer reallocation. Only the multiply pipeline exists; the desktop paper
  is white.

## 6. iPad bridge (`pendant-ffi`)

UniFFI proc macros (`uniffi::setup_scaffolding!("pendant")`), no UDL.

- `engine.rs`: `Core` (store, note registry, background sync; `create_note`,
  `open_note`, `set_sync_server(direct, token, fallback)`, `connect`,
  `suspend`, device registry) and `NoteSession` (text edits, title,
  sketches, `elements`, `begin_stroke` / `append_points` / `finish_stroke` /
  `cancel_stroke`, `finish_shape`, `erase_at`, `remove_element`). Events
  come back through the foreign traits `CoreListener` (`notes_changed`,
  `brushes_changed`, `sync_state`) and `NoteListener` (`synced`,
  `text_changed`, `strokes_changed`, `wet_begin` / `wet_points` /
  `wet_end` / `wet_cancel`, `assets_changed`). `Core` also owns the shared
  brush library (`list_brushes` / `upsert_brush` / `remove_brush`) and
  asset library (`list_assets` / `put_asset` / `remove_asset`) next to
  `builtin_brushes()`, `builtin_assets()` and the `brush_knobs` /
  `brush_with_knobs` / `brush_with_mask` / `brush_with_grain_image`
  helpers.
- `net.rs`: the single-socket sync task. Every direct path (desktop
  addresses plus mDNS finds) is dialled in parallel and the first handshake
  wins; the fallback relay is dialled only when none answers, and direct
  paths are re-probed while on the fallback.
- `brush.rs`: `BrushModeler` object (`push`, `predict`, `finish`, `update`,
  `pending_estimates`, `live_mesh`), `BrushRef`, `InkStyle` / `InkMesh`
  records, and free functions `stroke_mesh`, `points_mesh`, `element_mesh`,
  `shape_outline_mesh`, `hover_dab_mesh`, `recognize_shape`,
  `resize_shape`, `stroke_seed`. Meshes cross the boundary as byte buffers
  (`INK_VERTEX_FLOATS` floats per vertex) rather than element-wise lifts,
  which is what made 1000-point live strokes redraw in under 2 ms.
- `types.rs`: Swift-facing records and enums mirroring the core.
- `tests/engine.rs`: local persistence and a two-client round trip through
  the real relay router.

Build: `scripts/build-ios-core.sh` builds `staticlib` for
`aarch64-apple-ios` and `aarch64-apple-ios-sim`, runs library-mode bindgen
off the device archive, assembles `ios/PendantCore/PendantCoreFFI.xcframework`
and copies `Pendant.swift`. `scripts/swift-smoke.sh` compiles
`scripts/smoke/main.swift` against the host cdylib as a fast bindings check.

## 7. iPad app (`ios/Pendant`)

Generated with XcodeGen from `project.yml` (bundle `dev.darksailor.pendant`,
iOS 17, iPad and iPhone, `pendant://` URL scheme, camera, local network and
Bonjour usage strings, file sharing for recordings).

- `PendantApp.swift`, `AppModel.swift`: owns the UniFFI `Core`, the note
  list and one `NoteModel` per open note. Listener callbacks arrive on the
  Rust network thread and hop to the main actor. `-spike 1` and
  `-brushLab 1` replace the main UI.
- `MarkdownTextView.swift`: UITextView with a two-way CRDT binding.
  `SketchPreview.swift`: read-only preview with tappable sketch thumbnails.
- `SettingsScreen.swift`, `PairScreen.swift`, `ScanScreen.swift`,
  `RelayDiscovery.swift`: sync state, device registry, pairing QR out and in
  (VisionKit), Bonjour lookup of the desktop relay.
- `SketchScreen.swift`: the canvas. A `UIScrollView` owns finger pan, zoom
  and inertia over an empty content view; the Metal view sits above it
  pinned to the screen and reads the scroll state into its `Viewport`.
  `PenGestureRecognizer` captures pencil touches (coalesced and predicted)
  and `touchesEstimatedPropertiesUpdated`; `SketchModel` feeds them to the
  core `BrushModeler`, streams wet batches every 60 ms, draws the live mesh
  from `liveMesh`, coalesces estimate redraws to one per run-loop pass,
  waits up to a settle timeout for pending estimates before commit, arms
  draw-and-hold for shape snapping with a haptic, resizes a snapped shape on
  drag, erases by hit-testing elements in the core, and shows the hover dab
  from a `UIHoverGestureRecognizer`. Remote wet ink and `strokesChanged`
  diffs are deferred while a local pen is down. `StrokeRecorder` writes
  recorder v2 files under Documents when launched with `-recordStrokes 1`.
- `StrokeCodec.swift`: the PencilKit tool picker maps onto core presets
  (pen, pencil, marker, monoline, fountain; crayon → the bundled
  `builtin:crayon` at 1.5× width; watercolour → marker at 0.6 opacity) or,
  for a `PKToolPickerCustomItem`, a `BrushLibrary` brush. Picker state
  autosaves under `sketch`.
- `BrushLibrary.swift` + `BrushAttributesView.swift`: bundled brushes plus
  the workspace's (`brushesChanged`), one custom picker item each with the
  brush drawn as its icon and width swatches, and a popover of `BrushKnobs`
  sliders, tip-mask and paper menus and picture import, persisted per
  brush in `UserDefaults` (`brush.<id>.knobs|mask|grain`).
- `InkAssets.swift`: the mask and grain `r8` texture arrays (mipmapped)
  from the bundled PNGs and the workspace's assets (`assetsChanged`);
  `AssetImport` shrinks a picked picture to a greyscale PNG ≤ 64 KiB.
- `InkRenderer.swift` + `Shaders.metal`: `MTKViewDelegate`, demand-driven.
  Committed ink is one batched upload drawn as runs split only where the
  pipeline changes; wet, live, hover and settling strokes are separate
  geometries. Vertex stream is the core mesh plus a per-vertex stroke index
  into a `StrokeStyle` array (linear colour, mask, grain, depth slot,
  flags). Depth slots: committed `0.2 + 0.8·(N−k)/(N+1)`, wet `0.1 +
  0.001·(W−j)`, settling `0.05 − 0.001·i`, live `0.01`, hover `0.005`.
  Three pipelines: normal, multiply, screen. `darkPaper` (from the clear
  colour's linear luminance) swaps multiply for screen so a highlighter
  tints a black canvas instead of vanishing. Thumbnails render offscreen
  through the same pipeline at 2× with MSAA 4.
- `BrushLabScreen.swift`: preset grid (five tools × three widths from one
  canned stroke, EMA/ISM toggle), a PencilKit calibration page for the
  `widthScale` / `opacityScale` ratios (currently 1.0) and a corpus page
  that replays the bundled `brush/` recordings through every input model
  with their metrics (`-labPage`, `-labModel` preselect).

Launch arguments read from `UserDefaults`: `serverURL`, `altURLs`,
`fallbackURL`, `token`, `relayId`, `pairURI`, `spike`, `brushLab`, `labPage`, `labModel`,
`recordStrokes`, `tool`, `figureEight`, `fakeEstimates`, `pencilOnly`,
`anyInput`. UI tests take the relay from `PENDANT_TEST_SERVER` and
`PENDANT_TEST_TOKEN`.

UI tests (`UITests/`): `SketchUITests` (create and draw, remote stroke and
erase, marker self-overlap luminance, estimate settling, hold-to-shape,
embed preview tap, reopen keeps strokes, sidebar title, delete, bulk delete),
`SyncUITests`, `PairUITests`, `SpikeUITests`, `DeviceSpikeUITests`.

## 8. The shared rendering contract

Both shaders read the same `StrokeStyle` layout and flag bits: mask kind in
bits 0–1 (3 = ribbon edge feather on `|v|`), grain kind in bits 2–3 (1 =
value noise), `FLAG_GRAIN_STROKE` 16, `FLAG_MULTIPLY` 32, `FLAG_DISCARD` 64.
Both use the same `pcg2d` hash and value noise with the seed in an integer
slot, the same 0.75-unit stipple cell for the edge of write-once ink, and
linear-light premultiplied blending. Write-once ink is the depth trick: a
stroke's own later fragments at a sample fail the depth test, later strokes
sit strictly nearer and blend over. Any change to one shader is made to the
other in the same commit.

## 9. Building and verifying

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # core 85+ tests, ffi + server relay round trips
cargo fmt --all -- --check
nix flake check                              # clippy, fmt, toml-fmt, audit, deny, nextest, llvm-cov, docs
cargo run -p pendant                         # desktop with embedded relay
cargo run -p pendant-server -- --listen 127.0.0.1:8722 --db relay.redb --token demo

scripts/build-ios-core.sh                    # macOS: xcframework + Pendant.swift
scripts/swift-smoke.sh                       # macOS: bindings smoke
cd ios/Pendant && nix run nixpkgs#xcodegen   # regenerate Pendant.xcodeproj
xcodebuild -project Pendant.xcodeproj -scheme Pendant -destination 'platform=iOS Simulator,id=<udid>' build-for-testing
PENDANT_TEST_SERVER=ws://127.0.0.1:8722/ws PENDANT_TEST_TOKEN=demo xcodebuild test-without-building ... -only-testing:PendantUITests/SketchUITests
```

Golden meshes pin ink geometry; the brush and shape corpora replay real
iPad recordings; the marker UI test samples screenshot pixels. On-device
checks use `-recordStrokes 1` with the console attached, which prints an
`est:` line per stroke with estimate counts, redraw count and slowest
redraw.

## 10. Status and open items

Done: ink renderer plan (all phases), shape recogniser plan (all phases),
brush engine P0–P2 (specs, tip shapes, ink styles, estimate patching,
write-once marker and pencil, grain, hover dab, nib sweep by convex hull,
dark-paper highlighter, tilt corpus).

Open, in priority order:

- Brush engine P3 (stamped dabs, image masks and grains, workspace brush
  library, custom picker items) and P4 (`ink-stroke-modeler` trial behind
  an input-model trait). `InkMesh::zoom_independent` waits for P3.
- Pencil Pro hover dab unverified on hardware.
- PencilKit width and opacity calibration ratios still 1.0; measurement is
  by eye in the brush lab.
- Desktop has no dark theme, so only the multiply highlighter pipeline.
- Recogniser proptest `clean_shapes_are_recognised_equivariantly` has a
  known failing shrunk case (vertical line scaled and moved); reproduces
  before any brush work and is not in the regression file.
- `cargo mutants` not run; the desktop parity screenshot script does not
  exist.
