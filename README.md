# signal-agent

MCP Relay for Signal: let Claude Code, Claude Desktop, or any MCP client read and search your Signal messages, and send them if you allow it. It installs by double-click and runs entirely on your own machine.

Not affiliated with Signal Messenger or the Signal Foundation. Built on [presage](https://github.com/whisperfish/presage) and licensed under AGPL-3.0.

## Status

Early releases. The plan is [docs/PRD.md](docs/PRD.md) and the work is tracked in this repo's issues. Builds are on the [Releases](https://github.com/bluefishforsale/signal-agent/releases) page, and the app updates itself from there once installed, but only when you click to install.

## Install

- **macOS 13 or later:** download `signal-agent-<version>-macos-universal.dmg`, open it, drag the app to Applications, and open it. It's signed and notarized, so it opens without a Gatekeeper warning.
- **Windows 10 21H2 or later:** download `signal-agent-<version>-windows-x64-setup.exe`, or `-arm64-` on an ARM PC, and run it. It installs for your user only, with no admin prompt. It's signed, but Windows SmartScreen may still show "Windows protected your PC" while the publisher is new. If it does, choose More info, then Run anyway.
- **Linux (x86_64 or aarch64, glibc 2.31 or later):** unpack `signal-agent-<version>-linux-<arch>.tar.gz` and run `signal-agent setup`. The one-line installer is still to come.

Setup links this computer to your Signal account by QR code, like Signal Desktop does, brings over your message history, and registers itself with Claude Code and Claude Desktop.

## Read-only unless you say otherwise

Claude can read and search your messages but can't send anything as you, by default. To let it send, react and delete as you, open Advanced on the access screen during setup and tick "Allow Claude to send messages as you". In the terminal, answer y to "Let Claude send messages as you? [y/N]". Claude can send something you didn't mean to, so read what it's about to send before you approve it.

## Removing it

Use Uninstall under Advanced in the app, or `signal-agent uninstall` in a terminal. It removes the background agent, the Claude entries and the login item, and deletes your messages and keys only if you tick the box (or pass `--delete-data`).

## Building

The Rust workspace builds with `cargo build`. The Tauri app in `app/` builds on macOS and Windows; plain `cargo build` skips it so Linux needs no WebKit. Releases are signed in CI; [docs/signing-setup.md](docs/signing-setup.md) covers what that takes.
