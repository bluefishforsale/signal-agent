#!/bin/sh
# Installs MCP Relay for Signal's command-line agent for the current user on
# Linux and starts terminal setup. It is pinned to one release, and the
# download is checked against that release's SHA256SUMS before anything runs.
# No sudo: everything goes under ~/.local.
#
#   curl -fsSL https://github.com/bluefishforsale/signal-agent/releases/download/v0.1.0/install.sh | sh
#
# SIGNAL_AGENT_NO_SETUP=1 installs without starting setup.
set -eu

version=0.1.0
release_url=${SIGNAL_AGENT_RELEASE_URL:-https://github.com/bluefishforsale/signal-agent/releases/download/v$version}
bin_dir=$HOME/.local/bin

fail() {
  printf 'signal-agent install: %s\n' "$1" >&2
  exit 1
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https,file' --tlsv1.2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    fail "needs curl or wget to download"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    fail "needs sha256sum or shasum to verify the download"
  fi
}

os=$(uname -s)
[ "$os" = Linux ] || fail "this installer is for Linux, not $os. On macOS and Windows, download the app from https://github.com/bluefishforsale/signal-agent/releases"

machine=$(uname -m)
case $machine in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "there is no build for $machine; signal-agent ships for x86_64 and aarch64" ;;
esac

name=signal-agent-$version-linux-$arch
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading signal-agent $version for $arch..."
download "$release_url/$name.tar.gz" "$tmp/$name.tar.gz" || fail "unable to download $release_url/$name.tar.gz"
download "$release_url/SHA256SUMS" "$tmp/SHA256SUMS" || fail "unable to download $release_url/SHA256SUMS"

expected=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS lists no checksum for $name.tar.gz; nothing was installed"
actual=$(sha256 "$tmp/$name.tar.gz")
[ "$actual" = "$expected" ] || fail "$name.tar.gz does not match its published checksum; nothing was installed"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp" || fail "unable to unpack $name.tar.gz"
mkdir -p "$bin_dir"
# NOTE: copy beside the old binary and rename over it. Writing into the old
# file in place fails with "text file busy" while its agent is running.
cp "$tmp/$name/signal-agent" "$bin_dir/.signal-agent.new"
chmod 755 "$bin_dir/.signal-agent.new"
mv -f "$bin_dir/.signal-agent.new" "$bin_dir/signal-agent"
echo "Installed $bin_dir/signal-agent"

case :$PATH: in
  *:"$bin_dir":*) ;;
  *) echo "Add $bin_dir to your PATH to run signal-agent by name." ;;
esac

[ "${SIGNAL_AGENT_NO_SETUP:-}" = 1 ] && exit 0

# Setup asks questions, and under `curl | sh` stdin is the pipe, not the user.
if ! (: </dev/tty) 2>/dev/null; then
  fail "no terminal to run setup in. Run \"$bin_dir/signal-agent setup\" from an interactive shell"
fi
"$bin_dir/signal-agent" setup </dev/tty
