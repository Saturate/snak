#!/usr/bin/env bash
# Builds target/Snak.app: the binary, the icon from assets/icon.svg and an
# Info.plist, signed with the dev identity when it exists (scripts/dev-cert.sh).
# Usage: scripts/bundle.sh [--release] [--open]
set -euo pipefail
cd "$(dirname "$0")/.."

profile=debug
open_after=false
for arg in "$@"; do
  case "$arg" in
    --release) profile=release ;;
    --open) open_after=true ;;
  esac
done

if [ "$profile" = release ]; then
  cargo build -p snak-app --release
else
  cargo build -p snak-app
fi

app=target/Snak.app
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo run -q -p make-icon -- assets/icon.svg "$work/Snak.iconset"
iconutil -c icns "$work/Snak.iconset" -o "$work/Snak.icns"

# target/ is build output; the bundle is rebuilt from scratch every time.
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$profile/snak" "$app/Contents/MacOS/snak"
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
