#!/usr/bin/env bash
# Swift smoke test: build the host cdylib, generate Swift bindings, compile
# scripts/smoke/main.swift against them and run it. Needs swiftc, so on
# Linux it runs on the Mac build machine through scripts/on-mac.sh (M6 gate).
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  exec "$(dirname "$0")/on-mac.sh" "$(basename "$0")" "$@"
fi

root=$(git rev-parse --show-toplevel)
cd "$root"
gen="$root/target/swift-smoke"

cargo build -p krabink-ffi --lib
rm -rf "$gen"
mkdir -p "$gen"
cargo run -q -p krabink-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/debug/libkrabink_ffi.dylib --language swift --out-dir "$gen"
cp "$gen/krabinkFFI.modulemap" "$gen/module.modulemap"

# Clean env: a nix devShell's clang setup breaks Xcode's swiftc
# ("missing required module 'SwiftShims'").
env -i HOME="$HOME" PATH=/usr/bin:/bin TERM=dumb \
  xcrun swiftc -o "$gen/smoke" scripts/smoke/main.swift "$gen/krabink.swift" \
  -I "$gen" -L target/debug -lkrabink_ffi

DYLD_LIBRARY_PATH=target/debug "$gen/smoke"
