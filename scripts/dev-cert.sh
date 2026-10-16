#!/usr/bin/env bash
# Creates a self-signed "Snak Development" code-signing identity in the login
# keychain. Builds signed with it keep the same identity across rebuilds, so the
# Keychain's "Always Allow" sticks instead of prompting after every build.
set -euo pipefail

NAME="Snak Development"
if security find-identity -p codesigning | grep -q "$NAME"; then
  echo "\"$NAME\" already exists."
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -subj "/CN=$NAME" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null

# macOS's importer can't read OpenSSL 3's default PKCS#12 encryption.
pass=$(openssl rand -hex 16)
openssl pkcs12 -export -legacy -inkey "$work/key.pem" -in "$work/cert.pem" \
  -name "$NAME" -out "$work/identity.p12" -passout "pass:$pass"

security import "$work/identity.p12" -k "$HOME/Library/Keychains/login.keychain-db" \
  -P "$pass" -T /usr/bin/codesign

echo "Created \"$NAME\". cargo run now signs builds with it (see .cargo/config.toml)."
