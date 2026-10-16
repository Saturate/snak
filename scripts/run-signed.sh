#!/usr/bin/env bash
# Cargo runner: signs the binary with the stable dev identity, then runs it.
# Without the identity it runs the binary unsigned, as cargo would.
set -euo pipefail

binary="$1"
shift
if security find-identity -p codesigning 2>/dev/null | grep -q "Snak Development"; then
  # A fixed identifier plus a fixed certificate is what the Keychain keys
  # "Always Allow" on; the default ad-hoc signature changes every build.
  codesign --force --sign "Snak Development" --identifier dev.snak.app "$binary" 2>/dev/null
fi
exec "$binary" "$@"
