#!/usr/bin/env bash
# The macOS release runner is a persistent Mac, not a throwaway VM, so the
# keychain and notarization key macos-signing-setup.sh made must not outlive
# the job. delete-keychain also drops it from the user's search list.
set -euo pipefail

keychain="$RUNNER_TEMP/release.keychain-db"
if [[ -e "$keychain" ]]; then
  security delete-keychain "$keychain"
fi
rm -f "$RUNNER_TEMP/developer-id.p12" "$RUNNER_TEMP/notarization-key.p8"
