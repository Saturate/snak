#!/usr/bin/env bash
# Cargo runner: signs the binary with the stable dev identity, then runs it.
# Without the identity, or if signing fails, it runs the binary as built.
set -euo pipefail

binary="$1"
shift
# Test binaries never touch the login, so they don't get the app's identity.
case "$binary" in
  */deps/*) exec "$binary" "$@" ;;
esac
# No -v: the identity is self-signed, so macOS lists it as untrusted, and -v
# would hide it. codesign signs with it regardless.
if security find-identity -p codesigning 2>/dev/null | grep -q "Snak Development"; then
  # A fixed identifier plus a fixed certificate is what the Keychain keys
  # "Always Allow" on; the default ad-hoc signature changes every build.
  if ! codesign --force --sign "Snak Development" --identifier dev.snak.app "$binary" 2>/tmp/snak-codesign.err; then
    echo "run-signed: codesign failed, running unsigned (Keychain may prompt):" >&2
    cat /tmp/snak-codesign.err >&2
  fi
fi
exec "$binary" "$@"
