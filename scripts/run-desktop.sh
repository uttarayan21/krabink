#!/usr/bin/env bash
# `cargo run` for the desktop app, for paseo's "run-desktop" script and by
# hand. Paseo's script runner inherits the paseo server's environment, which
# on Linux is a user service with no graphical session variables, so winit
# panics with "neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is
# set". Point it at the running compositor's socket (or Xwayland) first.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [[ "$(uname)" == Linux && -z "${WAYLAND_DISPLAY:-}" && -z "${DISPLAY:-}" ]]; then
  export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
  sock=$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -print -quit 2>/dev/null || true)
  if [[ -n "$sock" ]]; then
    export WAYLAND_DISPLAY=$(basename "$sock")
  elif [[ -S /tmp/.X11-unix/X0 ]]; then
    export DISPLAY=:0
  else
    echo "error: no Wayland socket in $XDG_RUNTIME_DIR and no X display; log in graphically first" >&2
    exit 1
  fi
  echo "==> display: WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-} DISPLAY=${DISPLAY:-}"
fi

# Loro logs every encoded block at INFO; keep the terminal readable.
export RUST_LOG=${RUST_LOG:-info,loro_internal=warn,wgpu=error,naga=warn}

# The toolchain's linker (`cc`) and Bevy's system libs come from the flake
# devShell; paseo's env has neither, so enter the shell when needed.
run=(cargo run -r -p krabink -- "$@")
if ! command -v cc >/dev/null 2>&1; then
  if command -v direnv >/dev/null 2>&1; then
    run=(direnv exec . "${run[@]}")
  else
    run=(nix develop --command "${run[@]}")
  fi
fi
exec "${run[@]}"
