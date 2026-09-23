#!/usr/bin/env bash
# Build the iPad app for a physical device, install it, and launch it.
# Wired up as the "run-ipad" script in paseo.json; also fine to run by hand.
# On Linux the whole thing runs on the Mac build machine (scripts/on-mac.sh).
#
#   KRABINK_IPAD_ID   devicectl device UDID (default: first connected iPad,
#                     else the known iPad Pro 11 M4)
#   KRABINK_TEAM      DEVELOPMENT_TEAM for automatic signing
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "$@"
fi

root=$(git rev-parse --show-toplevel)
cd "$root"

team=${KRABINK_TEAM:-YD2FVR5QH2}
bundle=dev.darksailor.krabink
app_dir=ios/Krabink
derived="$app_dir/build-device"

device=${KRABINK_IPAD_ID:-}
if [[ -z "$device" ]]; then
  device=$(xcrun devicectl list devices 2>/dev/null \
    | awk '/iPad/ && /connected/ { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9A-F-]{36}$/) { print $i; exit } }')
fi
device=${device:-E894963B-F801-5AFA-B709-3F61603301BA}
echo "==> target device $device"

# Rust core -> XCFramework + Swift bindings (gitignored, so fresh worktrees need it).
if [[ ! -d ios/KrabinkCore/KrabinkCoreFFI.xcframework ]]; then
  echo "==> building KrabinkCore xcframework"
  scripts/build-ios-core.sh
fi

# Xcode project is generated from project.yml and gitignored.
scripts/gen-xcodeproj.sh

# Over ssh there is no GUI keychain session, so codesign cannot reach the
# signing identity until the keychains holding it are unlocked (see the
# ios-device-signing notes). A GUI login already has them open; then this
# is a no-op.
if [[ -f "$HOME/.keychain-pw" ]]; then
  pw=$(<"$HOME/.keychain-pw")
  for kc in login rscad-codesign; do
    f="$HOME/Library/Keychains/$kc.keychain-db"
    if [[ -f "$f" ]]; then
      security unlock-keychain -p "$pw" "$f" || echo "warning: could not unlock $kc keychain" >&2
    fi
  done
fi

echo "==> building for device"
xcodebuild \
  -project "$app_dir/Krabink.xcodeproj" \
  -scheme Krabink \
  -configuration Debug \
  -destination "platform=iOS,id=$device" \
  -destination-timeout 180 \
  -derivedDataPath "$derived" \
  -allowProvisioningUpdates \
  DEVELOPMENT_TEAM="$team" \
  CODE_SIGN_STYLE=Automatic \
  build

app="$derived/Build/Products/Debug-iphoneos/Krabink.app"
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
