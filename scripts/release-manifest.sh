#!/usr/bin/env bash
# Turns the collected installers into the rest of a release: Tauri updater
# signatures, the latest.json the in-app updater polls, and SHA256SUMS.
#
# NOTE: run this only after every OS signature is applied. Authenticode-signing
# an installer after its updater signature exists changes the bytes and the
# updater would reject the download.
#
# Usage: release-manifest.sh <dist dir> <version> <download base url>
# Expects TAURI_CLI (the tauri command) and an updater key in TAURI_SIGNING_*.
set -euo pipefail

dist=$1
version=$2
base_url=$3

cd "$dist"
mac="signal-agent-$version-macos-universal.app.tar.gz"
win_x64="signal-agent-$version-windows-x64-setup.exe"
win_arm64="signal-agent-$version-windows-arm64-setup.exe"

for payload in "$mac" "$win_x64" "$win_arm64"; do
  $TAURI_CLI signer sign --app-version "$version" "$payload" >/dev/null
done

# The universal app serves both Mac architectures.
jq -n \
  --arg version "$version" \
  --arg pub_date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg base "$base_url" \
  --arg mac "$mac" --arg mac_sig "$(cat "$mac.sig")" \
  --arg win_x64 "$win_x64" --arg win_x64_sig "$(cat "$win_x64.sig")" \
  --arg win_arm64 "$win_arm64" --arg win_arm64_sig "$(cat "$win_arm64.sig")" \
  '{
    version: $version,
    pub_date: $pub_date,
    platforms: {
      "darwin-aarch64": {url: "\($base)/\($mac)", signature: $mac_sig},
      "darwin-x86_64": {url: "\($base)/\($mac)", signature: $mac_sig},
      "windows-x86_64": {url: "\($base)/\($win_x64)", signature: $win_x64_sig},
      "windows-aarch64": {url: "\($base)/\($win_arm64)", signature: $win_arm64_sig}
    }
  }' >latest.json

files=(*)
sha256sum "${files[@]}" >SHA256SUMS
cat SHA256SUMS
