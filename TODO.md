# pendant — TODO

All milestones M0–M6 (desktop/server/FFI) and iM0–iM5 (iPad) are committed.
This file tracks the remaining backlog and the known debt logged during each
milestone. Nothing here blocks the core product; it already syncs live across
desktop + iPad with pen-to-desktop wet ink at p95 71ms on hardware.

## Backlog (M7 / ongoing)

- [ ] Presence cursors (who's editing / where, over the ephemeral channel).
- [ ] Shallow-snapshot GC (compact old CRDT history, bound store growth).
- [ ] `pendant export` CLI (markdown + SVG assets bundle to disk).
- [ ] Config UI (server URL / token) instead of launch args + UserDefaults.
- [ ] Desktop-side sketch editing (currently view-only on desktop).

## Known debt

### Sync / transport
- [ ] No `wss://` TLS on client sockets (desktop + FFI). Fine behind a reverse
      proxy; add native TLS before direct internet exposure.
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

### Desktop
- [ ] Cursor can jump when a remote edit lands while typing in the same note
      (buffer rebuilt; egui clamps). Cursor remap through remote deltas queued.

## Verification still worth doing (non-blocking)
- [ ] Hardware feel-check of the iM5 preview/tap-to-open flow on a real iPad.
- [ ] Real cross-device latency with NTP-synced clocks (current numbers are
      indicative — measured across iPad/mac clocks on one LAN).
