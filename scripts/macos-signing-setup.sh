#!/usr/bin/env bash
# A fresh macOS runner has no signing identity. This puts the Developer ID
# certificate in a throwaway keychain that codesign can use without a prompt, and
# writes the notarization key to disk for notarytool and Tauri.
set -euo pipefail

keychain="$RUNNER_TEMP/release.keychain-db"
cert="$RUNNER_TEMP/developer-id.p12"
api_key="$RUNNER_TEMP/notarization-key.p8"

base64 --decode <<<"$APPLE_CERTIFICATE" >"$cert"
base64 --decode <<<"$APPLE_API_PRIVATE_KEY" >"$api_key"

security create-keychain -p "$KEYCHAIN_PASSWORD" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$keychain"
security import "$cert" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASSWORD" "$keychain" >/dev/null
# shellcheck disable=SC2046 # the existing list is one path per line
security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
rm "$cert"

if ! security find-identity -v -p codesigning "$keychain" | grep -qF "\"$APPLE_SIGNING_IDENTITY\""; then
  echo "::error::APPLE_CERTIFICATE does not contain the identity named in APPLE_SIGNING_IDENTITY"
  exit 1
fi

echo "APPLE_API_KEY_PATH=$api_key" >>"$GITHUB_ENV"
