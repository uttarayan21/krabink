#!/usr/bin/env bash
# Build the iPad app for a physical device, install it, and launch it.
# Wired up as the "deploy-ipad" script in paseo.json; also fine to run by hand.
#
#   PENDANT_IPAD_ID   devicectl device UDID (default: first paired iPad, else
#                     the known iPad Pro 11 M4)
#   PENDANT_TEAM      DEVELOPMENT_TEAM for automatic signing
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "error: device deploys need macOS (xcodebuild/devicectl)" >&2
  exit 1
fi

root=$(git rev-parse --show-toplevel)
cd "$root"

team=${PENDANT_TEAM:-YD2FVR5QH2}
bundle=dev.darksailor.pendant
app_dir=ios/Pendant
derived="$app_dir/build-device"

device=${PENDANT_IPAD_ID:-}
if [[ -z "$device" ]]; then
  device=$(xcrun devicectl list devices 2>/dev/null \
    | awk '/iPad/ && /paired/ { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9A-F-]{36}$/) { print $i; exit } }')
fi
device=${device:-E894963B-F801-5AFA-B709-3F61603301BA}
echo "==> target device $device"

# Rust core -> XCFramework + Swift bindings (gitignored, so fresh worktrees need it).
if [[ ! -d ios/PendantCore/PendantCoreFFI.xcframework ]]; then
  echo "==> building PendantCore xcframework"
  scripts/build-ios-core.sh
fi

# Xcode project is generated from project.yml and gitignored.
if [[ ! -d "$app_dir/Pendant.xcodeproj" ]]; then
  echo "==> generating Pendant.xcodeproj"
  if command -v xcodegen >/dev/null; then
    (cd "$app_dir" && xcodegen generate)
  else
    (cd "$app_dir" && nix run nixpkgs#xcodegen -- generate)
  fi
fi

echo "==> building for device"
xcodebuild \
  -project "$app_dir/Pendant.xcodeproj" \
  -scheme Pendant \
  -configuration Debug \
  -destination "platform=iOS,id=$device" \
  -destination-timeout 180 \
  -derivedDataPath "$derived" \
  -allowProvisioningUpdates \
  DEVELOPMENT_TEAM="$team" \
  CODE_SIGN_STYLE=Automatic \
  build

app="$derived/Build/Products/Debug-iphoneos/Pendant.app"
[[ -d "$app" ]] || { echo "error: $app not found after build" >&2; exit 1; }

# devicectl is flaky right after a reconnect (NWError 60, launch 10002/4000);
# a short wait and retry succeeds.
retry() {
  local n
  for n in 1 2 3; do
    if "$@"; then return 0; fi
    echo "   attempt $n failed, retrying in 10s" >&2
    sleep 10
  done
  "$@"
}

echo "==> installing"
retry xcrun devicectl device install app --device "$device" "$app"

echo "==> launching $bundle"
retry xcrun devicectl device process launch --device "$device" --terminate-existing "$bundle"
echo "==> done"
