#!/usr/bin/env bash
# Archive the iPad app (Release) and export it for App Store Connect /
# TestFlight, or upload it straight away. Wired up as "archive-ios" in
# paseo.json. On Linux the whole thing runs on the Mac build machine
# (scripts/on-mac.sh); the build number is taken from git here first,
# because the Mac mirror only has a throwaway repo.
#
#   scripts/archive-ios.sh                # export ios/Pendant/build-archive/export/Pendant.ipa
#   PENDANT_EXPORT_DESTINATION=upload scripts/archive-ios.sh   # upload to ASC
#
#   PENDANT_TEAM               DEVELOPMENT_TEAM (needs a paid Apple Developer
#                              Program membership for app-store-connect)
#   PENDANT_EXPORT_METHOD      app-store-connect (default) | release-testing
#                              (ad hoc) | debugging (development)
#   PENDANT_EXPORT_DESTINATION export (default) | upload
#   PENDANT_BUILD_NUMBER       CFBundleVersion (default: commit count)
#   PENDANT_MARKETING_VERSION  CFBundleShortVersionString (default: project.yml)
#   PENDANT_ASC_KEY_PATH, PENDANT_ASC_KEY_ID, PENDANT_ASC_ISSUER_ID
#                              App Store Connect API key for upload without
#                              an Xcode account session (path is on the Mac)
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  export PENDANT_BUILD_NUMBER=${PENDANT_BUILD_NUMBER:-$(git rev-list --count HEAD)}
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "$@"
fi

root=$(git rev-parse --show-toplevel)
cd "$root"

team=${PENDANT_TEAM:-YD2FVR5QH2}
method=${PENDANT_EXPORT_METHOD:-app-store-connect}
destination=${PENDANT_EXPORT_DESTINATION:-export}
build=${PENDANT_BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}
version=${PENDANT_MARKETING_VERSION:-}
app_dir=ios/Pendant
derived="$app_dir/build-archive"
archive="$derived/Pendant.xcarchive"
export_dir="$derived/export"
options="$derived/ExportOptions.plist"

# The store build must carry the current core: always rebuild the
# XCFramework (cargo caches, so an unchanged core is quick).
echo "==> building PendantCore xcframework"
scripts/build-ios-core.sh
scripts/gen-xcodeproj.sh

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

echo "==> archiving Pendant (build $build${version:+, version $version})"
rm -rf "$archive"
xcodebuild \
  -project "$app_dir/Pendant.xcodeproj" \
  -scheme Pendant \
  -configuration Release \
  -destination "generic/platform=iOS" \
  -archivePath "$archive" \
  -derivedDataPath "$derived" \
  -allowProvisioningUpdates \
  DEVELOPMENT_TEAM="$team" \
  CODE_SIGN_STYLE=Automatic \
  CURRENT_PROJECT_VERSION="$build" \
  ${version:+MARKETING_VERSION="$version"} \
  archive

# manageAppVersionAndBuildNumber=false keeps the build number we set above
# instead of letting Xcode bump it to whatever ASC last saw.
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
	<true/>
	<key>manageAppVersionAndBuildNumber</key>
	<false/>
</dict>
</plist>
EOF

auth=()
if [[ -n "${PENDANT_ASC_KEY_PATH:-}" ]]; then
  auth=(
    -authenticationKeyPath "$PENDANT_ASC_KEY_PATH"
    -authenticationKeyID "$PENDANT_ASC_KEY_ID"
    -authenticationKeyIssuerID "$PENDANT_ASC_ISSUER_ID"
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
  echo "==> uploaded build $build to App Store Connect"
else
  ipa=$(find "$export_dir" -name '*.ipa' | head -n 1)
  echo "==> exported ${ipa:-$export_dir}"
  echo "    upload: xcrun altool --upload-app -f <ipa> -t ios --apiKey <id> --apiIssuer <issuer>"
  echo "    or rerun with PENDANT_EXPORT_DESTINATION=upload"
fi
