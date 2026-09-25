# krabink — TODO

All milestones M0–M6 (desktop/server/FFI) and iM0–iM5 (iPad) are committed.
This file tracks the remaining backlog and the known debt logged during each
milestone. Nothing here blocks the core product; it already syncs live across
desktop + iPad with pen-to-desktop wet ink at p95 71ms on hardware.

## Backlog (M7 / ongoing)

- [ ] Presence cursors: the iPad pen shows as a pointer on the desktop
      (`WetInk::Pointer`); still open: desktop mouse → iPad, who's editing
      text, per-device names on the pointer.
- [ ] Shallow-snapshot GC (compact old CRDT history, bound store growth).
- [ ] `krabink export` CLI (markdown + SVG assets bundle to disk).
- [ ] Config UI (relay URL / token) instead of launch args + UserDefaults.
- [ ] Publish `{endpoint_id, relay}` in `DeviceMeta` so nodes dial every
      workspace device without pairwise QRs (today: scanner dials QR owner,
      everyone dials the replica).
- [ ] Desktop-side drawing (both ink layers are view-only on desktop; no
      insert-sketch there either).
- [ ] Page ink export: group `page_elements()` by resolved line and emit
      an SVG after each paragraph (`export.rs` skips page ink today).
- [ ] Transparent ink overlay above the text (premultiplied output; the
      marker's multiply blend needs a different formulation) so ink can
      sit over text instead of under it.
- [ ] Peers' pointers on the iPad (`PointerAnchored` is desktop-only).
- [ ] Custom brush picker items (chalk, grainy pencil, workspace brushes)
      are off: `makePicker` skips `BrushLibrary.pickerItems()` until the
      icons read as tools and the colour follows dark mode like PencilKit's.

## Known debt

### Sync / transport
- [ ] Second redb on every device: the node mirrors docs in `node.redb`
      next to the app's own store (`krabink.redb`). Accepted for now (idle
      unload 300 s, compaction every 1000 updates); later make the FFI
      `State` and desktop `Docs` implement `DocProvider` directly.
- [ ] Peer `Fatal` (bad token) is sticky until `set_peers` / `resume`; no
      retry timer, no UI action to re-pair besides scanning again.
- [ ] mDNS is desktop-to-desktop and iPad-to-desktop only; an iPad never
      advertises, so two iPads on a relay-less LAN cannot find each other.
- [ ] Note close/unsubscribe not exposed over FFI — open notes stay subscribed
      for the session.
- [ ] Change events are coarse (full-text + page-length, not deltas). Fine
      until a consumer needs cursor-stable patches.

### iPad
- [x] Stroke identity keyed by `PKStrokePath.creationDate` — gone: PencilKit is
      input-only, ink layers are keyed by CRDT stroke id.
- [ ] Eraser is whole-stroke only (core hit test); PencilKit's pixel eraser
      (stroke splitting) is not reproduced.
- [ ] Lasso tool does nothing (no PKDrawing to select from).
- [ ] Marker translucency not round-tripped through the stroke schema.
- [ ] IME / marked-text composition not guarded in the remote-apply path of the
      text editor.
- [ ] Whole-text restyle after every keystroke (`MarkdownStyler.restyle`);
      fine to ~50 KB, an incremental restyle is the follow-up.
- [ ] Ink anchors are per source line, first fragment: ink on a wrapped
      continuation drifts when the width changes (iPad and desktop widths
      differ).
- [ ] Inline sketches draw only in the reading view (the editor shows the
      embed line as text); ink is not clipped to its box, so an old
      full-screen sketch gives a tall box with ink wider than the text.
      Also: an embed
      must be alone on its line, a repeated id shows one box, the box
      height depends on decodable brush specs (a build without the spec
      lays the text below out differently), and on the desktop a
      container narrower than ~200 pt wraps the hidden embed line and
      doubles the box.
- [ ] Reading view renders no images or tables (source shows through).
- [ ] Wet ink carries no per-point size yet; receivers (desktop and iPad, same
      ribbon code) fall back to `base_width * force` until the commit lands.

### Core
- [ ] `shape::tests::clean_shapes_are_recognised_equivariantly` fails on the
      case proptest saved in `crates/krabink-core/proptest-regressions/shape.txt`
      (found 2026-09-22 while running the suite; `shape.rs` unchanged). A
      wide, shallow arc keeps its kind after scale + translate but its
      bounds drift ~10 units past the tolerance (`shape.rs:2388`).

### Desktop
- [ ] Cursor can jump when a remote edit lands while typing in the same note
      (buffer rebuilt; egui clamps). Cursor remap through remote deltas queued.
- [ ] No bundled bold face: `Strong` is full-strength colour against a
      softened body instead of a heavier weight.

## Verification still worth doing (non-blocking)
- [ ] Hardware feel-check of the unified page on a real iPad: Pencil over
      text never scrolls or triggers Scribble, finger scrolls and places the
      caret, hover dab, picker survives keyboard dismissal, ink follows its
      line while typing above it, drawing in the reading view.
- [ ] Real cross-device latency with NTP-synced clocks (current numbers are
      indicative — measured across iPad/mac clocks on one LAN).
