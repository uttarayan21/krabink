#!/usr/bin/env bash
# Compile the iPad app for the simulator: no signing, no device. The quick
# "does it still build" check for the Swift side; from Linux it runs on the
# Mac through scripts/on-mac.sh.
#
#   KRABINK_SIM_ID   simulator UDID to target (default: generic simulator,
#                    which needs no booted device)
#   KRABINK_CONFIGURATION  Debug (default) or Release, to check the store
#                    build (iPad-only, dev screens compiled out)
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "$@"
fi

root=$(git rev-parse --show-toplevel)
cd "$root"
app_dir=ios/Krabink

if [[ ! -d ios/KrabinkCore/KrabinkCoreFFI.xcframework ]]; then
  echo "==> building KrabinkCore xcframework"
  scripts/build-ios-core.sh
fi
scripts/gen-xcodeproj.sh

if [[ -n "${KRABINK_SIM_ID:-}" ]]; then
  destination="platform=iOS Simulator,id=$KRABINK_SIM_ID"
else
  destination="generic/platform=iOS Simulator"
fi

echo "==> building for simulator"
set -o pipefail
xcodebuild \
  -project "$app_dir/Krabink.xcodeproj" \
  -scheme Krabink \
  -configuration "${KRABINK_CONFIGURATION:-Debug}" \
  -destination "$destination" \
  -derivedDataPath "$app_dir/build" \
  CODE_SIGNING_ALLOWED=NO \
  ARCHS=arm64 \
  build 2>&1 | grep -E "error:|warning: .*Sources/|\*\* BUILD"
# ARCHS=arm64: the generic simulator destination also wants x86_64, and the
# KrabinkCore xcframework only carries an arm64 simulator slice.
