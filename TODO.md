# pendant — TODO

All milestones M0–M6 (desktop/server/FFI) and iM0–iM5 (iPad) are committed.
This file tracks the remaining backlog and the known debt logged during each
milestone. Nothing here blocks the core product; it already syncs live across
desktop + iPad with pen-to-desktop wet ink at p95 71ms on hardware.

## Backlog (M7 / ongoing)

- [ ] Presence cursors (who's editing / where, over the ephemeral channel).
- [ ] Shallow-snapshot GC (compact old CRDT history, bound store growth).
- [ ] `pendant export` CLI (markdown + SVG assets bundle to disk).
- [ ] Config UI (relay URL / token) instead of launch args + UserDefaults.
- [ ] Publish `{endpoint_id, relay}` in `DeviceMeta` so nodes dial every
      workspace device without pairwise QRs (today: scanner dials QR owner,
      everyone dials the replica).
- [ ] Desktop-side sketch editing (currently view-only on desktop).

## Known debt

### Sync / transport
- [ ] Second redb on every device: the node mirrors docs in `node.redb`
      next to the app's own store (`pendant.redb`). Accepted for now (idle
      unload 300 s, compaction every 1000 updates); later make the FFI
      `State` and desktop `Docs` implement `DocProvider` directly.
- [ ] Peer `Fatal` (bad token) is sticky until `set_peers` / `resume`; no
      retry timer, no UI action to re-pair besides scanning again.
- [ ] mDNS is desktop-to-desktop and iPad-to-desktop only; an iPad never
      advertises, so two iPads on a relay-less LAN cannot find each other.
- [ ] Note close/unsubscribe not exposed over FFI — open notes stay subscribed
      for the session.
- [ ] Change events are coarse (full-text + per-sketch, not deltas). Fine until
      a consumer needs cursor-stable patches.

### iPad
- [x] Stroke identity keyed by `PKStrokePath.creationDate` — gone: PencilKit is
      input-only, ink layers are keyed by CRDT stroke id.
- [ ] Eraser is whole-stroke only (core hit test); PencilKit's pixel eraser
      (stroke splitting) is not reproduced.
- [ ] Lasso tool does nothing (no PKDrawing to select from).
- [ ] Remotely-created empty sketch is invisible until its first stroke lands.
- [ ] Marker translucency not round-tripped through the stroke schema.
- [ ] IME / marked-text composition not guarded in the remote-apply path of the
      text editor.
- [ ] No markdown syntax highlighting in the source editor.
- [ ] Preview is a separate read-only mode, not inline editable images — the
      UITextView↔CRDT offset mapping requires view text == CRDT text.
- [ ] Wet ink carries no per-point size yet; receivers (desktop and iPad, same
      ribbon code) fall back to `base_width * force` until the commit lands.

### Core
- [ ] `shape::tests::clean_shapes_are_recognised_equivariantly` fails on the
      case proptest saved in `crates/pendant-core/proptest-regressions/shape.txt`
      (found 2026-09-22 while running the suite; `shape.rs` unchanged). A
      wide, shallow arc keeps its kind after scale + translate but its
      bounds drift ~10 units past the tolerance (`shape.rs:2388`).

### Desktop
- [ ] Cursor can jump when a remote edit lands while typing in the same note
      (buffer rebuilt; egui clamps). Cursor remap through remote deltas queued.

## Verification still worth doing (non-blocking)
- [ ] Hardware feel-check of the iM5 preview/tap-to-open flow on a real iPad.
- [ ] Real cross-device latency with NTP-synced clocks (current numbers are
      indicative — measured across iPad/mac clocks on one LAN).
