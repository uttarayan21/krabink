#!/usr/bin/env bash
# Build the PendantCore XCFramework + generated Swift bindings for the iOS app.
# macOS only (needs xcodebuild); run from anywhere inside the repo, ideally in
# `nix develop` so the rust toolchain has the iOS targets.
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "error: iOS builds need macOS (xcodebuild)" >&2
  exit 1
fi

root=$(git rev-parse --show-toplevel)
cd "$root"
out="$root/ios/PendantCore"
gen="$root/target/uniffi-ios"
profile=release

targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for target in "${targets[@]}"; do
  # staticlib only: no link step, so the (macOS-targeting) nix cc-wrapper and
  # its libiconv never get involved. The cdylib crate-type would try to link
  # a per-target dylib nobody needs on iOS.
  cargo rustc -p pendant-ffi --release --target "$target" --crate-type staticlib
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
