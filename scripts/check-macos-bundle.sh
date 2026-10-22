#!/usr/bin/env bash
# Fails the build if the .app is missing what the login agent and Claude's
# config depend on: a universal signal-agent at Contents/MacOS/signal-agent and
# L4's SMAppService plist in Contents/Library/LaunchAgents pointing at it.
set -euo pipefail

app=$1
agent="$app/Contents/MacOS/signal-agent"
plist="$app/Contents/Library/LaunchAgents/com.bluefishforsale.signalagent.agent.plist"

[[ -x "$agent" ]] || { echo "::error::$agent is missing or not executable"; exit 1; }
archs=$(lipo -archs "$agent")
[[ " $archs " == *" x86_64 "* && " $archs " == *" arm64 "* ]] || { echo "::error::$agent is not universal (has: $archs)"; exit 1; }

[[ -f "$plist" ]] || { echo "::error::$plist is missing"; exit 1; }
plutil -lint "$plist"
program=$(plutil -extract BundleProgram raw "$plist")
[[ "$program" == Contents/MacOS/signal-agent ]] || { echo "::error::plist BundleProgram is $program, not Contents/MacOS/signal-agent"; exit 1; }

echo "bundle ok: signal-agent ($archs) and the login agent plist are in $app"
