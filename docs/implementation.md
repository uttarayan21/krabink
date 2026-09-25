# Krabink: stack and implementation

State of the tree on branch `unify-text-draw-canvas` (2026-09-24). This
is the map: what the stack is, where each piece lives,
and how the pieces talk to each other. The design rationale lives in the
plans (`docs/plans/*.md`, each with per-phase implementation notes) and the
sync topology in `docs/architecture.md`; this document points at them
rather than repeating them.

## 1. Stack at a glance

| Layer | Technology | Where |
|---|---|---|
| Language, toolchain | Rust, edition 2024, workspace resolver 3; nix flake (crane) for CI, dev shells and packages | `Cargo.toml`, `flake.nix` |
| Document model | Loro 1.13 CRDT wrapped so no Loro type escapes; postcard for wire and chunk encoding; ULID ids; Loro stable cursors as ink anchors | `crates/krabink-core/src/{note,workspace,sync_doc,ids,stroke,element}.rs` |
| Markdown | pulldown-cmark 0.13 (strikethrough, task lists) → style runs over the source and a reading-view text with a source map | `crates/krabink-core/src/markdown.rs` |
| Persistence | redb 4 (`snapshots` + `updates` tables keyed by `DocKey(u128)`) | `crates/krabink-core/src/store.rs` |
| Sync protocol | sans-io state machines, one version byte + postcard frame; QUIC lanes or in-process channels underneath | `crates/krabink-core/src/sync.rs` |
| Ink | `BrushSpec` presets, EMA input model, tip evaluator, lyon 1.0 stroker and a convex-hull nib sweeper, one `InkMesh` type | `crates/krabink-core/src/{brush,geom}/` |
| Shapes | deterministic draw-and-hold recogniser (ShortStraw corners, PCA fits) | `crates/krabink-core/src/shape.rs` |
| Sync node | iroh 1.2 `Endpoint` (hole punching, relay fallback), redb mirror of every doc, hub fan-out, mDNS (`_krabink._udp`, feature `mdns`) | `crates/krabink-local` |
| Cloud | iroh relay (`iroh-relay` server, workspace-token access control) + headless replica node in one binary | `crates/krabink-server` |
| Desktop | Bevy 0.19.1 (`wayland`), bevy_egui 0.42, custom `Material2d` + WGSL; runs a `krabink-local` node in-process | `crates/krabink` |
| iPad bridge | UniFFI 0.32 proc-macro bindings, staticlib per iOS target, XCFramework + generated `Krabink.swift` in a local SPM package | `crates/krabink-ffi`, `ios/KrabinkCore` |
| iPad app | SwiftUI + UIKit, iOS 17 deployment target, Metal renderer (4x MSAA, sRGB, depth), PencilKit only as the tool picker, VisionKit QR scanner, Bonjour discovery; XcodeGen project | `ios/Krabink` |

## 2. Repository layout

```
Cargo.toml              virtual workspace: crates/*
flake.nix               crane checks (clippy, fmt, toml-fmt, audit, deny, nextest, llvm-cov, docs), packages, dev shells
crates/krabink-core     platform-free core (no async, no UI); ~8.4k lines
crates/krabink-local    sync node library (iroh endpoint, hub, lanes, mDNS); compiles for iOS
crates/krabink-server   `krabink-server` binary: iroh relay + replica node
crates/krabink          desktop binary (Bevy) running a node, `pair` and `replay` subcommands
crates/krabink-ffi      UniFFI surface for Swift, `uniffi-bindgen` bin behind the `bindgen` feature
ios/KrabinkCore         SPM package: generated XCFramework + Krabink.swift (outputs of scripts/build-ios-core.sh)
ios/Krabink             XcodeGen spec (project.yml), Sources/, UITests/
scripts/build-ios-core.sh   cargo rustc for aarch64-apple-ios{,-sim} → bindgen → xcodebuild -create-xcframework
scripts/swift-smoke.sh      host cdylib + bindgen + scripts/smoke/main.swift, run on macOS
docs/architecture.md    sync topology, wire, pairing, fan-out, cloud deployment, failure modes
docs/plans/             ink-renderer (done), shape-recognizer (done), brush-engine (P0–P2 done, P3–P4 planned)
.github/workflows       build.yaml (nix check matrix + llvm-cov → codecov), docs.yaml (cargo doc check)
```

## 3. Core crate (`krabink-core`)

Compiles unchanged for Linux, macOS and iOS. Everything platform-specific
sits above it.

### 3.1 Documents

- `NoteDoc` (`note.rs`): one Loro doc per note. Containers: `meta` (title),
  `text` (markdown) and the root movable list `page`: the note's ink, one
  map per element (`elem = stroke | shape`, the stroke or shape fields,
  and `anchor`). A root container exists implicitly, so two devices
  drawing offline before their first sync simply union. An **anchor** is
  an encoded Loro stable cursor on the first char of the source line the
  ink was drawn on: `anchor_at(char_index)` makes one, `resolve_anchor`
  turns it back into a unicode-scalar index in the current text (a
  deleted line resolves to its deletion point; the refreshed cursor is
  cached per anchor, since that resolution costs a Loro diff), and
  `page_elements()` returns `PageElement { element, anchor }` in z order
  with `add_page_stroke` / `add_page_shape` / `remove_page_element` /
  `page_len` beside it. Element points are stored relative to `(text
  container left edge, top of the line's first fragment)` at a 16 pt body
  font on every platform. The `sketches` container (per-sketch element
  lists under `strokes`: `create_sketch`, `sketch_ids`, `elements(sketch)`,
  `sketch_len`, `add_stroke` / `add_shape` / `remove_element`) is the
  **inline** layer: a sketch shown as a box in the text flow at its
  `![…](krabink://sketch/<id>)` line, drawn in place, points relative to
  the box's padded corner. `inline.rs` fixes the box contract:
  `INLINE_PADDING` (8), `INLINE_MIN_HEIGHT` (160) and
  `sketch_box_height(elements) = max(min, ceil(max_y of the outlines +
  2·padding))`, so every platform lays the same box out for the same
  elements (`outline_bounds` is shared with the exporter). Text splice,
  title and the CRDT triad `version`, `export_updates_since`,
  `import_update`, `export_snapshot` as before.
- `WorkspaceDoc` (`workspace.rs`): registry of notes (`NoteMeta`) and paired
  devices (`DeviceMeta`) so the library UI never opens every note.
- `SyncDoc` (`sync_doc.rs`): semantics-free import/export/version; what the
  relay holds.
- `Store` (`store.rs`): snapshot plus append-only update log per `DocKey`,
  with `Flush` control and `checkpoint`. Shared by server, desktop and iPad.
- Ids (`ids.rs`): `NoteId`, `SketchId`, `StrokeId`, `ElementId`, `DeviceId`,
  all ULIDs; `seed()` yields the low 32 bits for stroke-mapped grain.
- `export.rs`: markdown export rewriting `krabink://sketch/<id>` embeds to
  SVG assets rendered from the elements through `geom/outline.rs`. Page
  ink is not exported yet (it has no layout-independent position).
- `pair.rs`: `PairInfo` ⇄ `krabink://pair?server=…&token=…[&alt=…][&fallback=…][&relay=…]`.

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
- `WetInk` (`wetink.rs`): `BeginAnchored{stroke, anchor, tool, color,
  base_width, spec}` → `Points{seq, sent_ms, chunks}`* → `End{tail}` or
  `Cancel`. The points are the modelled stage-1 points in the anchor's
  space, so a receiver runs the identical fold and the commit replaces the
  provisional ink without a visible change. `PointerAnchored{anchor, x, y,
  tilt, tool, color, base_width, down}` / `PointerAnchoredGone` share the
  lane: where the sender's pen is (hovering or drawing; `tool == None` is
  the eraser), throttled to ~30 Hz on the iPad, keyed by the `from` device
  on receipt. The sketch-keyed `Begin{sketch, …}` / `Pointer{sketch, …}` /
  `PointerGone{sketch}` still decode (postcard tags are declaration order)
  and every receiver drops them. Variants are append-only; older receivers
  log-and-drop what they cannot decode.

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
  against `tests/golden/mesh.txt`; regenerate with `KRABINK_UPDATE_GOLDEN=1`.
- `corpus.rs` parses recorder v1 (4 columns) and v2 (7/8 columns with tilt
  and an `est` flag) files. `tests/corpus/shapes/` has seven recordings for
  `tests/shape_corpus.rs`; `tests/corpus/brush/` has fifteen iPad
  recordings (fountain, pencil, pen, marker, monoline) for
  `tests/brush_corpus.rs`, which measures every input model the build has
  (`corpus::StrokeMetrics`: jitter, lag, deviation from the raw path,
  overshoot, width range, vertex count, µs), asserts loose bounds and
  dumps rows with `KRABINK_METRICS_JSON=path`.
- `brush/ism.rs` (feature `ism`): the ink-stroke-modeler input model
  behind `InputModelKind::Ism` / `BrushModeler::with_model`; the P4 trial,
  not what the canvas uses (decision in `docs/plans/brush-engine.md`).

### 3.7 Markdown (`markdown.rs`)

Both editors show the markdown source itself, styled in place, so the
view text always equals the CRDT text. `style_runs(text)` parses with
pulldown-cmark (strikethrough and task lists on) and returns
`StyleRun { start, end, kind }` in unicode scalars: `Heading{level}`,
`Strong`, `Emphasis`, `Strikethrough`, `CodeSpan`, `CodeBlock`,
`ListItem{depth, ordered}`, `BlockQuote`, `Link`, `ThematicBreak`, and
`Marker` for the syntax itself (`# `, `**`, `- `, `1. `, `> `, fences,
backticks, `[`/`](url)`, `[ ]`), found by the gap rule: inside each block
span, whatever is not covered by a leaf event, split on whitespace. Runs
are sorted by start, outer before inner, and block kinds cover whole
source lines. `preview_text(text)` builds the reading view from the same
runs: `PreviewText { text, source_of, runs }` is the source with markers
dropped (bullets become `•` / `◦`, task boxes `☐` / `☑`, ordinal markers
and rules kept, fence-only lines removed), a display-char → source-char
map (one sentinel past the end) and the runs remapped onto the display
text. Both platforms translate ink anchors through `source_of` so the
same ink sits on the same line in either view.

`SketchEmbed { sketch }` marks an inline sketch: an image whose URL is
`krabink://sketch/` plus a 26-char ULID, alone on its source line (only
whitespace around it), for the first occurrence of that id. It is one
run over the whole `![…](…)` span and never hosts `Marker` or `Link`
runs, so both reading views hide the line as a unit. Anything else (a bare
URI, an embed sharing its line, a repeat of an id, a malformed id) stays
a `Link` with markers. The rule lives in the core so the two apps show
a box for exactly the same lines. `preview_text` needs nothing extra:
no char of an embed is dropped, the line survives and the run is
remapped 1:1. The box shows in the reading view only; the editor styles
the embed run as a link, so the line stays ordinary editable source.

## 4. Sync node (`krabink-local`) and cloud (`krabink-server`)

Every device runs one `krabink_local::Node`: an iroh `Endpoint` with a
persisted key (`node_key`), a redb mirror of every doc (`node.redb`,
`ServerDocs` from `docs.rs`, checkpointed every 30 s, idle docs unloaded)
and a `Hub` (peer registry, tokens, changed-gated fan-out). Inbound QUIC
connections and the app's in-process `LocalLink` are served by the same
`ServerSession` loop (`serve.rs`); outbound peers are driven by one dial
loop each (`outbound.rs`: backoff, `ClientSession`, route reporting).
Wire: ALPN `krabink/sync/1`, two lanes per connection (`framing.rs`).
`Node` API: `start`, `local_link`, `set_peers`, `set_relay`,
`add_addr_hint` (a new hint sends `OutCmd::Hint`, which cuts the dial
loop's backoff short), `add_token`, `peers` / `watch_peers`,
`relay_health`, `suspend` / `resume` / `network_changed`, `shutdown`.
`start` binds the endpoint before opening the store, so a taken
`bind_port` fails with nothing else held and the caller can retry.
`mdns.rs` (feature `mdns`, desktop only) advertises `_krabink._udp` with
TXT `id`, `port` and `addrs` (every candidate interface) and turns hits
into address hints. `tests/mesh.rs` runs three nodes with the relay
disabled.

`krabink-server` is one binary: `relay.rs` spawns an `iroh_relay` server
whose `TokenAccess` admits only workspace tokens; `replica.rs` starts a
`Role::Replica` node behind it (accepts every token holder, never dials,
pinned UDP port). `config.rs` reads the TOML shown in
`docs/architecture.md`; `--dev` is plain HTTP on `127.0.0.1:3340` with
the replica on and prints a pair URI. `tests/relay.rs` runs the real relay
on an ephemeral port: convergence through the relay, latency, denied
tokens, replica bridging offline edits.

## 5. Desktop (`krabink`)

Bevy app with egui UI. Modules:

- `config.rs`: data dir (store, `node.redb`, `node_key`, `node_port`
  (last bound UDP port, rebound on start so pairings and hints survive a
  restart), device id, per-install `workspace_token`) and
  `~/.config/krabink/config.toml`
  (`relay`, `token`, `replica`, `[[peers]]`), overridable by `cli.rs` flags
  (`--data-dir`, `--relay`, `--token`, `--follow-latest`). Subcommands:
  `completions`, `pair <uri>` (writes the pairing to config.toml),
  `replay` (headless 120 Hz latency rig dialling a `--pair` URI),
  `brush-lab`.
- `docs.rs`: workspace registry and open notes over the shared store.
- `node.rs`: the `krabink-local` node as a Bevy resource (`SyncNode`),
  mDNS advertise/browse feeding `add_addr_hint`, and the QR's direct
  addresses following the endpoint's.
- `sync.rs`: one `ClientSession` over the node's `LocalLink`, driven once
  per frame; the node does peers, relay and fan-out. See
  `docs/architecture.md`.
- `ui.rs`: library sidebar (a Select mode with checkbox rows and a
  two-step bulk delete, the iPad's edit mode; `Docs::delete_notes` drops
  the registry rows, note history stays in the store) and one styled
  editor per note: an egui
  `TextEdit` with a custom layouter that turns the core's style runs into
  a `LayoutJob` (16 pt body, heading sizes 16×{1.6, 1.4, 1.25, 1.1},
  markers muted, code monospace on the raised surface, list indents;
  `StyledCache` recomputes the runs once per edit), bound to the CRDT by
  prefix/suffix diffing on `changed()`. A "Preview" slide switch swaps the
  buffer for `preview_text` in the same read-only `TextEdit`
  (`PreviewCache`). Each frame the open note writes a `PageLayout`
  resource: the galley, the visible window, every page element's
  origin (`resolve_anchor` → `pos_from_cursor`, translated through the
  preview's source map when it is showing; unresolvable anchors go to the
  end of the text) and, in the reading view, the inline boxes. There an
  embed run is laid out as transparent 8 pt text on a row `line_height`
  = box height (glyphs top-aligned), so the box takes its place in the
  flow (`layout_job` takes the heights as `Some`; the editor passes
  `None` and shows the embed line as link text, so it can be edited and
  selected like any other line); `BoxHeights`
  caches `sketch_box_height` per sketch on the doc version, re-measuring
  only sketches whose element count moved. Right after the `TextEdit`
  lays out, `inline_boxes` reads each embed's row top and the frame
  (hairline border, "sketch" caption) is painted with the egui painter
  above the text; `PageLayout.boxes` / `box_origin` hand the padded
  corner to the scene. The page texture is painted under the text at the
  scrolled rect. `settings.rs`: sync state, devices, pairing QR,
  paste-to-join.
- `sketch.rs`: the page ink is one off-screen Bevy scene (own render layer
  and camera) rendered into an `Image` the size of the visible window
  (rounded to 64 px, 256 px vertical overscan, capped at 4096) with the
  paper colour as its opaque clear colour, so the marker's multiply blend
  has paper to multiply against. Element meshes are built once in anchor
  space and only their `Transform` moves when `PageLayout` changes.
  Two committed layers: inline elements (one per box the layout reports,
  placed at the box origin, despawned when the embed line goes) at z
  `k/100` capped at 400, overlay elements at `500 + k/100` capped at
  980; remote wet strokes at `990 + j/100` (900 slots) are dropped when
  the commit lands or on timeout. A wet stroke carries a `WetPlacement`
  (`Line(anchor)` from `BeginAnchored`, `Sketch(id)` from `Begin`) and is
  skipped while its line or box is not laid out. Peers' pens
  draw above that at 999.2 (the tool's hover dab, faint hovering /
  stronger drawing) and 999.4 (a monoline ring in a per-device hue); a
  pointer dies on `PointerAnchoredGone` or after 1.5 s of silence. The
  desktop shows both layers; it does not draw and cannot insert a sketch.
- `lab.rs`: `krabink brush-lab --corpus <file|dir> --presets … --models
  ema,ism --out dir --svg --metrics`, the tuning bench: SVG grids per
  recording through `elements_to_svg` and `metrics.json` from
  `corpus::StrokeMetrics`; the `ism` feature adds the ISM row.
- `ink_assets.rs`: `InkAssets` resource, the mask and grain `r8` texture
  arrays (CPU box mips, `image` crate decoding) from the bundled PNGs and
  the workspace's assets, rebuilt when `asset_ids()` changes; scenes
  respawn their strokes when its generation moves.
- `ink_material.rs` + `ink.wgsl`: the desktop twin of the Metal pipeline.
  One `Material2d` per `InkCombo` (blend × overlap, four for the page)
  sharing one `InkPalette` storage buffer of `StrokeStyle`; each mesh
  carries a `MeshTag` index into it plus the custom `ATTRIBUTE_INK_UV` and
  `ATTRIBUTE_INK_OPACITY` vertex attributes. Reversed-Z: accumulate runs
  compare `GreaterEqual`, discard runs `Greater` with depth write. The
  palette starts at 64 slots and pads to a power of two; same-size uploads
  rewrite the GPU buffer in place, and growth swaps in a fresh
  `ShaderBuffer` asset and repoints the materials at it, because a resized
  buffer is a new GPU resource the existing bind groups never see (touching
  the materials does not rebuild them). Only the multiply pipeline exists;
  the desktop paper is white.

## 6. iPad bridge (`krabink-ffi`)

UniFFI proc macros (`uniffi::setup_scaffolding!("krabink")`), no UDL.

- `engine.rs`: `Core` (store, the in-process `krabink-local` node, note
  registry; `create_note`, `open_note`, `set_pairing(PairInfo)`,
  `add_peer_addr`, `connect`, `suspend`, `network_changed`, `node_id`,
  `bound_port`, `peers`, `sync_state`, `pair_info`, device registry) and
  `NoteSession` (`text`, `apply_text_edit`, title; page ink: `anchor_at`,
  `resolve_anchor` / `resolve_anchors`, `page_elements` (each element with
  its anchor resolved to a `char_index`), `begin_page_stroke` /
  `append_points` / `finish_page_stroke` / `cancel_stroke`,
  `finish_page_shape`, `erase_page_at` (one probe per element, in that
  element's anchor space, hit-tested in the core), `remove_page_element`,
  `send_page_pointer` / `send_page_pointer_gone`; inline sketches:
  `sketch_ids`, `create_sketch`, `elements(sketch)`,
  `sketch_box_height(sketch)` (the minimum for an unknown container),
  `begin_stroke(sketch, …)` (streams `WetInk::Begin`), `finish_stroke` /
  `finish_shape`, `erase_at(sketch, x, y, radius)`, `remove_element`).
  Events come back through the foreign traits `CoreListener`
  (`notes_changed`, `brushes_changed`, `assets_changed`,
  `devices_changed`, `sync_state`) and `NoteListener` (`synced`,
  `text_changed`, `page_changed`, `strokes_changed(sketch)`, `wet_begin`
  (inline) / `wet_begin_anchored` (overlay) / `wet_points` / `wet_end` /
  `wet_cancel`). `page_changed` fires when an import changes the page
  list's length, `strokes_changed` when a sketch's element-list length
  moves (`sketch_len` diffed per sketch around the import; a container
  arriving fires once at length 0, before its first stroke lands).
  Elements are only ever added or removed whole. `Core` also owns the shared
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
- `markdown.rs`: `style_runs` and `preview_text` as free functions with
  `StyleRun` / `StyleKind` (`SketchEmbed { sketch: String }`) /
  `PreviewText` records, plus `inline_padding()` / `inline_min_height()`
  (UniFFI cannot export constants).
- `types.rs`: Swift-facing records and enums mirroring the core
  (`PageElement`, `PageProbe` among them).
- `tests/engine.rs`: local persistence, two-client round trips (direct and
  through the real relay router) including page ink and anchored wet ink,
  inline sketches (wet begin, `strokes_changed`, equal box heights,
  erase), eraser probes, style runs. `examples/probe.rs` is the UI-test
  harness's remote peer: `--expect`, `--append`, `--add-page-stroke LINE`,
  `--expect-page-elements N`, `--add-sketch-stroke EMBED` (nth embed in
  text order), `--expect-sketch-elements N`, `--wet-watch`, `--devices`.

Build: `scripts/build-ios-core.sh` builds `staticlib` for
`aarch64-apple-ios` and `aarch64-apple-ios-sim`, runs library-mode bindgen
off the device archive, assembles `ios/KrabinkCore/KrabinkCoreFFI.xcframework`
and copies `Krabink.swift`. `scripts/swift-smoke.sh` compiles
`scripts/smoke/main.swift` against the host cdylib as a fast bindings check.

## 7. iPad app (`ios/Krabink`)

Generated with XcodeGen from `project.yml` (bundle `dev.darksailor.krabink`,
iOS 17, iPad and iPhone, `krabink://` URL scheme, camera, local network and
Bonjour usage strings, file sharing for recordings).

- `KrabinkApp.swift`, `AppModel.swift`: owns the UniFFI `Core`, the note
  list and one `NoteModel` per open note (each with its `PageInkModel`,
  cached with the note). Listener callbacks arrive on the Rust network
  thread and hop to the main actor. `NoteDetail` is the note page on a
  card with an Edit / Preview toggle, insert-sketch (`newSketch`:
  `createSketch` first, its own commit, then the embed line spliced
  after the caret's line through `applyTextEdit`, flowing into the view
  like a remote edit; disabled in preview), erase-last, and on iOS 17 a
  brush sheet. `-spike 1` and `-brushLab 1` replace the main UI.
- `NoteCanvasView.swift`: the one surface per note. A TextKit 1
  `UITextView` (`usingTextLayoutManager: false`: eager, deterministic line
  fragments) over an opaque Metal view cleared to the paper colour, so
  ink renders under the text and the highlighter multiplies against
  paper. The CRDT binding is shadow-based: a local edit is diffed against
  the shadow (`TextSplice.of`), a remote text arriving while a local
  splice is pending is reconciled by transforming the local splice past
  the remote one (`TextSplice.transformed(past:)`) and rewriting the view
  from the merged CRDT text with the caret remapped, so neither side's
  keystrokes are lost or applied at stale offsets. In preview the view
  goes read-only and shows `previewText` restyled from its runs.
  `LineLayout.swift`: `ScalarIndex` (unicode scalar ↔ UTF-16) and
  `LineLayout`, the forward map scalar → top of its line's first fragment
  and the inverse point → line, both translated through the preview's
  `sourceOf` map so the ink model only ever sees source scalars.
  `MarkdownStyler.swift`: applies the core's style runs as TextKit
  attributes over the whole text after every edit (attribute-only edits
  do not fire `textViewDidChange` or move the selection). In the reading
  view a `SketchEmbed` run becomes a hidden row: 8 pt clear glyphs,
  paragraph line height pinned to the sketch's box height,
  `.byClipping`; in the editor it is link text (`boxHeights: nil`), so
  the line is edited and selected like any other. The canvas caches
  `sketchBoxHeight` per sketch (dropped when that sketch changes), and
  once each layout settles measures the boxes (`LineLayout.inlineBoxes`:
  x = the ink origin's left edge, top = the embed row's first fragment,
  width = the container's, height from the cache) and frames them with
  `InlineBoxOverlay`, a non-interactive subview of the text view (one
  `CAShapeLayer` border and `CATextLayer` "sketch" caption per box)
  drawn above ink and text. Leaving the reading view drops the boxes,
  and with them the inline ink, until the next toggle. The caret's
  source scalar is reported to the model on every selection change.
- `SettingsScreen.swift`, `PairScreen.swift`, `ScanScreen.swift`,
  `PeerDiscovery.swift`: peers and routes, device registry, pairing QR out
  and in (VisionKit), Bonjour lookup (`_krabink._udp`) of the paired
  desktop for the relay-less LAN: every `ip:port` from the TXT record
  (`addrs` + `port`) plus the UDP-resolved one become address hints.
- `PenInput.swift`: `PenGestureRecognizer` on the text view captures
  Pencil touches (coalesced and predicted) and
  `touchesEstimatedPropertiesUpdated`; every other recogniser on the text
  view must fail first, Scribble is refused, and fingers scroll and place
  the caret. On the simulator or under `-anyInput 1` a finger inks too
  (a short, still touch is a tap for the caret; two fingers scroll).
  `StrokeRecorder` writes recorder v2 files under Documents when launched
  with `-recordStrokes 1`.
- `PageInkModel.swift`: pen-down tests the inline boxes first
  (`inlineBox(at:)`, a rect test, since the paragraph spacing under the
  embed row still maps to that line) and otherwise asks the layout for
  the line under the pen; the stroke's `Placement` (`.sketch(id)` with
  the box's padded corner as origin, or `.line(anchor)`) picks the
  sketch-keyed or the page calls for begin, commit and wet streaming.
  Every sample is translated into that space; it feeds the core
  `BrushModeler`, streams wet batches
  every 60 ms, draws the live mesh from `liveMesh`, coalesces estimate
  redraws to one per run-loop pass, waits up to a settle timeout for
  pending estimates before commit, arms draw-and-hold for shape snapping
  with a haptic, resizes a snapped shape on drag, erases by hit-testing
  elements in the core (one probe per overlay element in reach, plus
  `eraseAt` for every box the eraser reaches into), and shows the hover
  dab from a `UIHoverGestureRecognizer`. `layoutChanged()` (coalesced
  from `NSLayoutManagerDelegate`) re-resolves every anchor and moves the
  placed, wet, settling and live ink to its line's new origin, so ink
  follows its line while typing above it, then `syncInline()` loads the
  sketches whose box appeared, moves the ones still there and drops the
  ones whose box went (their data stays in the note). Inline elements are
  shown below the overlay (z re-assigned inline-first after any change).
  Erase-last takes the newest element of either layer by `createdMs`.
  An inline wet stroke whose box is not on the page is dropped until its
  commit lands; while drawing inline, the pointer still rides the embed
  line's anchor. Remote wet ink, `pageChanged` diffs and
  `strokesChanged` reloads are deferred while a local pen is down. The
  status line the UI tests read: `strokes= shapes= wetSent= wetRecv=
  est= custom= originY= inline= boxY=` (`strokes=` counts the overlay
  only; `inline=` every box's elements; `boxY=` the first box's page y).
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
  flags). Every element carries its line origin; the offset is added
  while the mesh floats are copied into the batch, so moving a line
  re-uploads without re-tessellating. Depth slots: committed `0.2 +
  0.8·(N−k)/(N+1)`, wet `0.1 + 0.001·(W−j)`, settling `0.05 − 0.001·i`,
  live `0.01`, hover `0.005`.
  Three pipelines: normal, multiply, screen. `darkPaper` (from the clear
  colour's linear luminance) swaps multiply for screen so a highlighter
  tints a black canvas instead of vanishing. Thumbnails render offscreen
  through the same pipeline at 2× with MSAA 4.
- `BrushLabScreen.swift`: preset grid (five tools × three widths from one
  canned stroke, EMA/ISM toggle), a PencilKit calibration page for the
  `widthScale` / `opacityScale` ratios (currently 1.0) and a corpus page
  that replays the bundled `brush/` recordings through every input model
  with their metrics (`-labPage`, `-labModel` preselect).

Launch arguments read from `UserDefaults`: `pairURI`, `spike`, `brushLab`,
`labPage`, `labModel`, `recordStrokes`, `tool`, `figureEight`,
`fakeEstimates`, `pencilOnly`, `anyInput`. UI tests take the pairing URI
from `KRABINK_TEST_PAIR` (what `krabink-server --dev` prints).

UI tests (`UITests/`): `SketchUITests` (draw on the page, remote page
stroke and erase, marker self-overlap luminance, estimate settling,
hold-to-shape, custom brush round trip, reopen keeps strokes, sidebar
title, delete, bulk delete), `StyledEditorUITests` (source text preserved,
ink follows its line when a heading above it changes),
`PreviewUITests` (reading view hides markers and keeps the source),
`InlineSketchUITests` (insert a sketch, draw inside its box in the
reading view and below it, erase-last across layers, the box follows
text typed above it, inline ink survives reopening, the editor shows the
embed as text and the box comes back with the reading view),
`SyncUITests`,
`PairUITests`, `SpikeUITests`, `DeviceSpikeUITests`.

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

Inline sketch boxes share one geometry contract too: the box's top is the
top of the embed line's first fragment (the same rule as an overlay
anchor), its left edge is the ink origin's x, the sketch's (0, 0) is that
corner inset by `INLINE_PADDING` on both axes, and the height is the
core's `sketch_box_height` of the committed elements (recomputed on
commit or remote change, never mid-stroke; grows downward only). Width,
caption and paragraph spacing are cosmetic and may differ per platform.

## 9. Building and verifying

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # core, local mesh, server relay, ffi round trips
cargo fmt --all -- --check
nix flake check                              # clippy, fmt, toml-fmt, audit, deny, nextest, llvm-cov, docs
cargo run -p krabink                         # desktop node (LAN only without --relay)
cargo run -p krabink-server -- --dev         # relay + replica on 127.0.0.1:3340, prints a pair URI
cargo run -p krabink -- --data-dir /tmp/b pair '<uri>'   # second desktop joins

scripts/build-ios-core.sh                    # xcframework + Krabink.swift
scripts/swift-smoke.sh                       # bindings smoke
scripts/check-ipad.sh                        # simulator compile, no signing
scripts/deploy-ipad.sh                       # device build + install + launch (paseo: run-ipad)
scripts/gen-xcodeproj.sh                     # regenerate Krabink.xcodeproj from project.yml
xcodebuild -project Krabink.xcodeproj -scheme Krabink -destination 'platform=iOS Simulator,id=<udid>' build-for-testing
KRABINK_TEST_PAIR='krabink://pair?…' xcodebuild test-without-building ... -only-testing:KrabinkUITests/SketchUITests
```

The iOS scripts need Xcode. Run on Linux, each one hands itself to the Mac
build machine through `scripts/on-mac.sh`: the worktree is mirrored with
rsync to `~/Porject/krabink-<worktree>` on `shiro` (one folder per
worktree, `KRABINK_MAC_HOST` / `KRABINK_MAC_DIR` override) and the script
runs there over ssh. Build artefacts stay on the Mac between runs. Device
signing over ssh needs the keychains unlocked (`~/.keychain-pw` on the Mac,
handled by `deploy-ipad.sh`) and a signed-in Xcode account for
`-allowProvisioningUpdates`.

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
- Both apps ship the four Catppuccin flavours (Settings, "Appearance");
  the highlighter multiplies on Latte's light paper and screens on the
  dark ones, re-specialised on switch.
- Recogniser proptest `clean_shapes_are_recognised_equivariantly` has a
  known failing shrunk case (vertical line scaled and moved); reproduces
  before any brush work and is not in the regression file.
- `cargo mutants` not run; the desktop parity screenshot script does not
  exist.

Text and ink are one surface since `unify-text-draw-canvas`: ink is
anchored to source lines in the note's `page` list, both apps show the
styled source (markers visible) with a reading-view toggle, the iPad
draws and types, the desktop shows page ink read-only. Known limits:

- Anchors are per source line (first fragment): ink on a later wrapped
  fragment drifts when the width changes, and iPad and desktop widths
  differ.
- Inserting a newline exactly at a line start moves that line's ink down;
  typing elsewhere on the line does not.
- Inline sketches (`![…](krabink://sketch/<id>)`, the legacy `sketches`
  container) are boxes in the text flow of the reading view, drawn in
  place there on the iPad, view-only on the desktop; in the editor the
  embed is a plain source line. They export to SVG as before. Ink is not
  clipped to its box, so an old full-screen sketch gives a tall box with
  ink wider than the text. An embed must be alone on its line; a repeat
  of an id is plain link text; deleting the line hides the box (data
  kept). The box height depends on decodable brush specs: a build that
  cannot decode a custom spec lays the text below out differently
  (stored coordinates are unaffected). On the desktop a container
  narrower than about 200 pt wraps the hidden embed line and doubles the
  box.
- Page ink (the overlay) is not exported; the desktop does not draw or
  insert sketches; the iPad does not show peers' pointers.
- The reading view is the source with markers hidden: tables and raw
  HTML show as source, images are not rendered.
- The ink layer is opaque paper under the text (a transparent overlay
  would break the marker's multiply blend).
