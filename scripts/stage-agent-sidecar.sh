#!/usr/bin/env bash
# The app ships the signal-agent CLI beside its own executable: Claude's config
# launches it from there and the macOS login agent plist names it. Tauri's
# externalBin wants it under app/src-tauri/binaries with the target triple in
# the name, so this copies an already-built CLI into place.
#
# Usage: stage-agent-sidecar.sh [target triple | universal-apple-darwin]
# With no argument it stages a plain `cargo build --release -p signal-agent` for
# the host, which is what building or testing relay-app locally needs, since
# tauri-build refuses to compile the app without its sidecar in place.
# For universal-apple-darwin, both Apple targets must already be built.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/app/src-tauri/binaries"
mkdir -p "$dest"

if (($# == 0)); then
  host=$(rustc -vV | sed -n 's/^host: //p')
  ext=""
  [[ "$host" == *-windows-* ]] && ext=.exe
  cp "$root/target/release/signal-agent$ext" "$dest/signal-agent-$host$ext"
  ls -l "$dest"
  exit 0
fi
target=$1

case "$target" in
  # A universal build compiles the app once per arch, and tauri-build checks
  # for that arch's sidecar each time; only the bundler uses the fat binary.
  universal-apple-darwin)
    for arch in aarch64-apple-darwin x86_64-apple-darwin; do
      cp "$root/target/$arch/release/signal-agent" "$dest/signal-agent-$arch"
    done
    lipo -create -output "$dest/signal-agent-$target" \
      "$dest/signal-agent-aarch64-apple-darwin" "$dest/signal-agent-x86_64-apple-darwin"
    ;;
  *-windows-*)
    cp "$root/target/$target/release/signal-agent.exe" "$dest/signal-agent-$target.exe"
    ;;
  *)
    cp "$root/target/$target/release/signal-agent" "$dest/signal-agent-$target"
    ;;
esac
ls -l "$dest"
