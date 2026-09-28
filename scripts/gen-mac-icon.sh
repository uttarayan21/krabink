#!/usr/bin/env bash
# Render the Mac app icon set from the iPad icon (scripts/gen-app-icon.py
# output) with sips. The PNGs are committed under
# macos/Assets.xcassets/AppIcon.appiconset; rerun after regenerating the
# iPad icon. macOS only (sips).
set -euo pipefail

root=$(git rev-parse --show-toplevel)
src="$root/ios/Krabink/Assets.xcassets/AppIcon.appiconset/AppIcon.png"
catalog="$root/macos/Assets.xcassets"
out="$catalog/AppIcon.appiconset"
mkdir -p "$out"

cat > "$catalog/Contents.json" <<'EOF'
{
  "info" : {
    "author" : "xcode",
    "version" : 1
  }
}
EOF

images=()
for size in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$((size * scale))
    name="icon_${size}x${size}@${scale}x.png"
    sips -z "$px" "$px" "$src" --out "$out/$name" >/dev/null
    images+=("    { \"filename\" : \"$name\", \"idiom\" : \"mac\", \"scale\" : \"${scale}x\", \"size\" : \"${size}x${size}\" }")
  done
done

{
  echo '{'
  echo '  "images" : ['
  (IFS=$'\n'; echo "${images[*]}") | sed '$!s/$/,/'
  echo '  ],'
  echo '  "info" : {'
  echo '    "author" : "xcode",'
  echo '    "version" : 1'
  echo '  }'
  echo '}'
} > "$out/Contents.json"

echo "mac icon set: $out"
