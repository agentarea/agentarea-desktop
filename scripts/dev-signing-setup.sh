#!/bin/bash
# One-time: a self-signed code-signing identity for dev builds.
#
# `tauri dev` binaries are ad-hoc signed by the linker, so every rebuild is a
# "new app" to macOS: Keychain "Always Allow" and privacy grants (files,
# microphone, …) are forgotten and it asks again. Signing each build with one
# stable identity (see dev-run.sh) keeps those grants across rebuilds.
set -euo pipefail
NAME="AgentArea Dev Signing"
KC="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "$NAME"; then
  echo "\"$NAME\" is already set up."; exit 0
fi

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/cert.cnf" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
  -out "$tmp/id.p12" -passout pass:aa-dev
# -T: codesign may use the key without a prompt each build.
security import "$tmp/id.p12" -k "$KC" -P aa-dev -T /usr/bin/codesign
echo "Trusting it for code signing (macOS asks for your password once)…"
security add-trusted-cert -r trustRoot -p codeSign -k "$KC" "$tmp/cert.pem"
security find-identity -v -p codesigning | grep "$NAME"
echo "Done. Restart \`pnpm tauri dev\`; answer macOS's prompts once with \"Always Allow\"."
