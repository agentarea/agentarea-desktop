#!/bin/bash
# Cargo runner (src-tauri/.cargo/config.toml): signs the app binary with the
# stable dev identity before running it, so macOS keeps Keychain and privacy
# grants across rebuilds. No identity (see dev-signing-setup.sh) → runs as is.
NAME="AgentArea Dev Signing"
if [[ "$(basename "$1")" == "agentarea-desktop" ]] &&
   security find-identity -v -p codesigning 2>/dev/null | grep -q "$NAME"; then
  codesign -f -s "$NAME" -i com.jamakase.agentarea-desktop "$1" 2>/dev/null ||
    echo "dev-run: could not sign $1; running it unsigned" >&2
fi
exec "$@"
