#!/usr/bin/env bash
# Turns the collected installers into the rest of a release: Tauri updater
# signatures, the latest.json the in-app updater polls, and SHA256SUMS.
#
# NOTE: run this only after every OS signature is applied. Authenticode-signing
# an installer after its updater signature exists changes the bytes and the
# updater would reject the download.
#
# Usage: release-manifest.sh <dist dir> <version> <download base url>
# Expects TAURI_CLI (the tauri command), an updater key in TAURI_SIGNING_*, and
# minisign. UPDATER_PUBKEY overrides the public key the app ships with, for a
# dry run signed with a throwaway key.
set -euo pipefail

pubkey=${UPDATER_PUBKEY:-$(jq -r .plugins.updater.pubkey "$(dirname "$0")/../app/src-tauri/tauri.conf.json")}

dist=$1
version=$2
base_url=$3

cd "$dist"
mac="signal-agent-$version-macos-universal.app.tar.gz"
win_x64="signal-agent-$version-windows-x64-setup.exe"
win_arm64="signal-agent-$version-windows-arm64-setup.exe"

# NOTE: the installed app accepts an update only if its signature checks out
# against the pubkey in tauri.conf.json. Verifying with that same key here
# turns a key mismatch into a failed release instead of an update nobody can
# install.
base64 -d <<<"$pubkey" >updater.pub
for payload in "$mac" "$win_x64" "$win_arm64"; do
  $TAURI_CLI signer sign --app-version "$version" "$payload" >/dev/null
  base64 -d <"$payload.sig" >"$payload.minisig"
  minisign -Vq -p updater.pub -m "$payload" -x "$payload.minisig"
  rm "$payload.minisig"
done
rm updater.pub

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
