#!/usr/bin/env bash
# (Re)generate ios/Krabink/Krabink.xcodeproj from project.yml when it is
# missing, older than the spec, or the set of source files changed (xcodegen
# lists every file explicitly, so a new .swift file needs a regenerate).
# macOS only; the build scripts call this.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
app_dir="$root/ios/Krabink"
proj="$app_dir/Krabink.xcodeproj/project.pbxproj"
stamp="$app_dir/Krabink.xcodeproj/.sources-stamp"

sources=$(cd "$app_dir" && find Sources UITests -type f | LC_ALL=C sort)
if [[ -f "$proj" && ! "$app_dir/project.yml" -nt "$proj" && -f "$stamp" ]] \
  && [[ "$(cat "$stamp")" == "$sources" ]]; then
  exit 0
fi
echo "==> generating Krabink.xcodeproj"
if command -v xcodegen >/dev/null; then
  (cd "$app_dir" && xcodegen generate)
else
  (cd "$app_dir" && nix run nixpkgs#xcodegen -- generate)
fi
printf '%s' "$sources" > "$stamp"
