#!/usr/bin/env bash
# Archive the desktop app for the Mac App Store and export a signed .pkg,
# or upload it straight away. Wired up as "archive-macos" in paseo.json.
# Same shape as archive-ios.sh: on Linux it runs on the Mac through
# scripts/on-mac.sh, with the build number taken from git here first.
#
#   scripts/archive-macos.sh              # export macos/build-archive/export/Krabink.pkg
#   KRABINK_EXPORT_DESTINATION=upload scripts/archive-macos.sh   # upload to ASC
#
#   KRABINK_TEAM               DEVELOPMENT_TEAM
#   KRABINK_EXPORT_METHOD      app-store-connect (default) | developer-id
#   KRABINK_EXPORT_DESTINATION export (default) | upload
#   KRABINK_BUILD_NUMBER       CFBundleVersion (default: commit count)
#   KRABINK_MARKETING_VERSION  CFBundleShortVersionString (default: project.yml)
#   KRABINK_ASC_KEY_PATH, KRABINK_ASC_KEY_ID, KRABINK_ASC_ISSUER_ID
#                              App Store Connect API key (see archive-ios.sh)
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  export KRABINK_BUILD_NUMBER=${KRABINK_BUILD_NUMBER:-$(git rev-list --count HEAD)}
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "$@"
fi

root=$(git rev-parse --show-toplevel)
cd "$root"

team=${KRABINK_TEAM:-YD2FVR5QH2}
method=${KRABINK_EXPORT_METHOD:-app-store-connect}
destination=${KRABINK_EXPORT_DESTINATION:-export}
build=${KRABINK_BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}
version=${KRABINK_MARKETING_VERSION:-}
app_dir=macos
derived="$app_dir/build-archive"
archive="$derived/Krabink.xcarchive"
export_dir="$derived/export"
options="$derived/ExportOptions.plist"
universal=target/universal-apple-darwin/release

# Must match LSMinimumSystemVersion (project.yml deploymentTarget).
export MACOSX_DEPLOYMENT_TARGET=12.0

# Apple's clang for both mac targets, like build-ios-core.sh does for iOS:
# the nix devshell's cc-wrapper only links for the host arch.
sdk=$(xcrun --sdk macosx --show-sdk-path)
clang=$(xcrun --sdk macosx --find clang)
export SDKROOT="$sdk"
for triple in aarch64_apple_darwin x86_64_apple_darwin; do
  upper=$(tr '[:lower:]' '[:upper:]' <<<"$triple")
  export "CC_$triple=$clang"
  export "AR_$triple=$(xcrun --find ar)"
  export "CARGO_TARGET_${upper}_LINKER=$clang"
done

echo "==> building krabink (aarch64 + x86_64)"
targets=(aarch64-apple-darwin x86_64-apple-darwin)
for target in "${targets[@]}"; do
  cargo build -p krabink --release --target "$target"
done
mkdir -p "$universal"
lipo -create \
  "target/aarch64-apple-darwin/release/krabink" \
  "target/x86_64-apple-darwin/release/krabink" \
  -output "$universal/krabink"
lipo -info "$universal/krabink"

echo "==> generating macos/Krabink.xcodeproj"
if command -v xcodegen >/dev/null; then
  (cd "$app_dir" && xcodegen generate --quiet)
else
  (cd "$app_dir" && nix run nixpkgs#xcodegen -- generate --quiet)
fi

# Same headless-keychain dance as deploy-ipad.sh.
if [[ -f "$HOME/.keychain-pw" ]]; then
  pw=$(<"$HOME/.keychain-pw")
  for kc in login rscad-codesign; do
    f="$HOME/Library/Keychains/$kc.keychain-db"
    if [[ -f "$f" ]]; then
      security unlock-keychain -p "$pw" "$f" || echo "warning: could not unlock $kc keychain" >&2
    fi
  done
fi

echo "==> archiving Krabink for macOS (build $build${version:+, version $version})"
rm -rf "$archive"
xcodebuild \
  -project "$app_dir/Krabink.xcodeproj" \
  -scheme Krabink \
  -configuration Release \
  -destination "generic/platform=macOS" \
  -archivePath "$archive" \
  -derivedDataPath "$derived" \
  -allowProvisioningUpdates \
  DEVELOPMENT_TEAM="$team" \
  CODE_SIGN_STYLE=Automatic \
  CURRENT_PROJECT_VERSION="$build" \
  ${version:+MARKETING_VERSION="$version"} \
  archive

# No dSYM (the binary is not linked by Xcode), so no symbol upload.
cat > "$options" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>method</key>
	<string>$method</string>
	<key>destination</key>
	<string>$destination</string>
	<key>teamID</key>
	<string>$team</string>
	<key>signingStyle</key>
	<string>automatic</string>
	<key>uploadSymbols</key>
	<false/>
	<key>manageAppVersionAndBuildNumber</key>
	<false/>
</dict>
</plist>
EOF

auth=()
if [[ -n "${KRABINK_ASC_KEY_PATH:-}" ]]; then
  auth=(
    -authenticationKeyPath "$KRABINK_ASC_KEY_PATH"
    -authenticationKeyID "$KRABINK_ASC_KEY_ID"
    -authenticationKeyIssuerID "$KRABINK_ASC_ISSUER_ID"
  )
fi

echo "==> exporting ($method, $destination)"
rm -rf "$export_dir"
xcodebuild -exportArchive \
  -archivePath "$archive" \
  -exportOptionsPlist "$options" \
  -exportPath "$export_dir" \
  -allowProvisioningUpdates \
  "${auth[@]}"

if [[ "$destination" == upload ]]; then
  echo "==> uploaded macOS build $build to App Store Connect"
else
  pkg=$(find "$export_dir" -name '*.pkg' | head -n 1)
  echo "==> exported ${pkg:-$export_dir}"
  echo "    rerun with KRABINK_EXPORT_DESTINATION=upload to send it to App Store Connect"
fi
