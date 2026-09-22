#!/usr/bin/env bash
# Run one of the repo's macOS-only scripts on the Mac build machine.
#
#   scripts/on-mac.sh deploy-ipad.sh [args…]
#
# Mirrors this worktree to the Mac with rsync (one remote folder per
# worktree; build artefacts already on the Mac are kept between runs) and
# runs the script there, streaming its output. The macOS-only scripts call
# this themselves when started on Linux, so `paseo run run-ipad` works from
# either side.
#
#   PENDANT_MAC_HOST  ssh host (default: shiro)
#   PENDANT_MAC_DIR   remote checkout, relative to the remote $HOME
#                     (default: Porject/pendant-<worktree dir name>, or
#                     Porject/pendant when the checkout is named pendant)
#   PENDANT_IPAD_ID, PENDANT_TEAM, PENDANT_SIM_ID  forwarded to the script
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: scripts/on-mac.sh <script in scripts/> [args…]" >&2
  exit 2
fi
script=$1
shift
root=$(git rev-parse --show-toplevel)
if [[ ! -x "$root/scripts/$script" ]]; then
  echo "error: scripts/$script is not an executable script" >&2
  exit 2
fi

host=${PENDANT_MAC_HOST:-shiro}
name=$(basename "$root")
dir=${PENDANT_MAC_DIR:-}
if [[ -z "$dir" ]]; then
  if [[ "$name" == pendant ]]; then dir=Porject/pendant; else dir="Porject/pendant-$name"; fi
fi

echo "==> syncing to $host:$dir"
ssh "$host" "mkdir -p '$dir'"
# The Mac ships openrsync (no --filter, no --info), so plain excludes only.
# Excluded paths are never deleted remotely: cargo's target/, the generated
# Xcode project and the built xcframework survive between runs.
rsync -az --delete \
  --exclude .git --exclude target --exclude .direnv --exclude 'result*' \
  --exclude ios/Pendant/build --exclude 'ios/Pendant/build-*' \
  --exclude ios/Pendant/Pendant.xcodeproj --exclude ios/Pendant/Info.plist \
  --exclude ios/PendantCore/PendantCoreFFI.xcframework \
  --exclude ios/PendantCore/Sources --exclude ios/PendantCore/.build \
  "$root/" "$host:$dir/"

env=""
for var in PENDANT_IPAD_ID PENDANT_TEAM PENDANT_SIM_ID; do
  if [[ -n "${!var:-}" ]]; then env+=" $var=$(printf %q "${!var}")"; fi
done
args=""
for arg in "$@"; do args+=" $(printf %q "$arg")"; done

# The mirror has no .git (a worktree's .git is a pointer into the main
# checkout). Nix flake commands on the Mac want a git tree, so keep a
# throwaway repo there with everything staged.
remote="cd '$dir' && { [ -d .git ] || git init -q; } && git add -A && env$env scripts/$script$args"
if [[ -t 0 ]]; then
  exec ssh -t "$host" "$remote"
else
  exec ssh "$host" "$remote"
fi
