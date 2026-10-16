#!/usr/bin/env bash
# Builds Snak.app in the cargo target dir: the binary, the icon from assets/icon.svg and an
# Info.plist, signed with the dev identity when it exists (scripts/dev-cert.sh).
# Usage: scripts/bundle.sh [--release] [--open] [--target <triple>]
set -euo pipefail
cd "$(dirname "$0")/.."

profile=debug
open_after=false
target=""
while [ $# -gt 0 ]; do
  case "$1" in
    --release) profile=release ;;
    --open) open_after=true ;;
    --target)
      [ $# -ge 2 ] || { echo "bundle.sh: --target needs a value" >&2; exit 2; }
      target="$2"; shift ;;
    *) echo "bundle.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

build_args=(-p snak-app)
[ "$profile" = release ] && build_args+=(--release)
[ -n "$target" ] && build_args+=(--target "$target")
cargo build "${build_args[@]}"

target_dir=$(cargo metadata --format-version 1 --no-deps \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")
app="$target_dir/Snak.app"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo run -q -p make-icon -- assets/icon.svg "$work/Snak.iconset"
iconutil -c icns "$work/Snak.iconset" -o "$work/Snak.icns"

# The target dir is build output; the bundle is rebuilt from scratch every time.
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# With --target, cargo puts the binary under a per-target folder.
binary_dir="$target_dir/$profile"
[ -n "$target" ] && binary_dir="$target_dir/$target/$profile"
cp "$binary_dir/snak" "$app/Contents/MacOS/snak"
cp "$work/Snak.icns" "$app/Contents/Resources/Snak.icns"
version=$(cargo metadata --format-version 1 --no-deps \
  | python3 -c "import json,sys; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name']=='snak-app'))")

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Snak</string>
  <key>CFBundleDisplayName</key><string>Snak</string>
  <key>CFBundleIdentifier</key><string>dev.snak.app</string>
  <key>CFBundleExecutable</key><string>snak</string>
  <key>CFBundleIconFile</key><string>Snak</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.social-networking</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

if security find-identity -p codesigning 2>/dev/null | grep -q "Snak Development"; then
  # Same identity as scripts/run-signed.sh, so Keychain approvals carry over.
  codesign --force --sign "Snak Development" --identifier dev.snak.app "$app"
fi

echo "Built $app"
if $open_after; then
  open "$app"
fi
