#!/usr/bin/env bash
# Build the PendantCore XCFramework + generated Swift bindings for the iOS app.
# Needs xcodebuild, so on Linux it runs on the Mac build machine through
# scripts/on-mac.sh. Run from anywhere inside the repo; the rust toolchain
# there needs the aarch64-apple-ios{,-sim} targets (rustup or `nix develop`).
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "$@"
fi

root=$(git rev-parse --show-toplevel)
cd "$root"
out="$root/ios/PendantCore"
gen="$root/target/uniffi-ios"
profile=release

# `ring` (iroh's TLS) builds C with `cc`; the nix devshell's CC targets
# macOS, so point each iOS target at the matching Apple clang + SDK.
ios_sdk=$(xcrun --sdk iphoneos --show-sdk-path)
sim_sdk=$(xcrun --sdk iphonesimulator --show-sdk-path)
export CC_aarch64_apple_ios="$(xcrun --sdk iphoneos --find clang)"
export AR_aarch64_apple_ios="$(xcrun --find ar)"
export CFLAGS_aarch64_apple_ios="-isysroot $ios_sdk -target arm64-apple-ios17.0"
export CC_aarch64_apple_ios_sim="$(xcrun --sdk iphonesimulator --find clang)"
export AR_aarch64_apple_ios_sim="$(xcrun --find ar)"
export CFLAGS_aarch64_apple_ios_sim="-isysroot $sim_sdk -target arm64-apple-ios17.0-simulator"
export IPHONEOS_DEPLOYMENT_TARGET=17.0

targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for target in "${targets[@]}"; do
  # staticlib only: no link step, so the (macOS-targeting) nix cc-wrapper and
  # its libiconv never get involved. The cdylib crate-type would try to link
  # a per-target dylib nobody needs on iOS.
  cargo rustc -p pendant-ffi --release --features ism --target "$target" --crate-type staticlib
done

# Library-mode bindgen off one static lib (the metadata is target-independent).
rm -rf "$gen"
mkdir -p "$gen"
cargo run -q -p pendant-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library "target/${targets[0]}/$profile/libpendant_ffi.a" \
  --language swift --out-dir "$gen"

# XCFramework wants a headers dir with a `module.modulemap`.
headers="$gen/include"
mkdir -p "$headers"
cp "$gen/pendantFFI.h" "$headers/"
cp "$gen/pendantFFI.modulemap" "$headers/module.modulemap"

rm -rf "$out/PendantCoreFFI.xcframework"
xcodebuild -create-xcframework \
  -library "target/aarch64-apple-ios/$profile/libpendant_ffi.a" -headers "$headers" \
  -library "target/aarch64-apple-ios-sim/$profile/libpendant_ffi.a" -headers "$headers" \
  -output "$out/PendantCoreFFI.xcframework"

mkdir -p "$out/Sources/PendantCore"
cp "$gen/pendant.swift" "$out/Sources/PendantCore/Pendant.swift"

echo "PendantCore ready: $out"
