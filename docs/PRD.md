# PRD: MCP Relay for Signal (repo `signal-agent`) v2.0

Status: approved 2026-10-21. Implementation is scoped to this document; changes to goals or anti-goals come back here first.

## Summary

A single Rust program, built on [presage](https://github.com/whisperfish/presage), that lets Claude Code, Claude Desktop, or any MCP client read, search and send your Signal messages. Double-click install on macOS and Windows with setup entirely in a native window; a one-line command on Linux. It replaces the v1 kit's signal-cli (Java) plus signal-mcp (Python) stack, with no Java, Python, Homebrew, WSL, Docker or Signal Desktop dependency. Public repo `bluefishforsale/signal-agent`, AGPL-3.0. Display name **MCP Relay for Signal**; repo and binary are `signal-agent`.

## Users

The primary user is non-technical: they can download a file, double-click it, and scan a QR code with their phone, and nothing more. The secondary user is technical: headless Linux, the HTTP endpoint, several accounts.

## Goals

1. **Installers.** macOS: universal `.app` in a `.dmg`, signed and notarized, macOS 13+. Windows: per-user NSIS `.exe` (no admin prompt), signed with Azure Artifact Signing, x64 and ARM64, Windows 10 21H2+. Linux: terminal one-liner, x86_64 and aarch64, glibc 2.31+. Signing identity: individual, Terrac Skiens.
2. **Release pipeline.** A `vX.Y.Z` tag builds, signs, and publishes every installer to GitHub Releases. The release job refuses to publish anything unsigned. Every push builds unsigned test installers as CI artifacts.
3. **Native setup.** Tauri v2 window on macOS and Windows; the same steps as a terminal flow on Linux. Both are front ends over one setup state machine in the core. Steps:
   - Account type: link my account (default), or give the agent its own number.
   - Linking: QR code that refreshes itself until scanned; a readable device name on the phone, e.g. "Claude (Terrac's MacBook)".
   - Registration (own number): the captcha page opens inside the window and its `signalcaptcha://` result is captured automatically (Linux: paste); then the SMS or voice code.
   - Access: a plain-language screen, "Let Claude send messages as you?", Yes preselected, one line on the risk. No means read-only.
   - History import via link-and-sync.
   - Claude registration for Claude Code and Claude Desktop, ending with a "Restart Claude Desktop" button.
   - The app moves itself to Applications before setup on macOS, so client configs never point into a translocated path.
4. **History.** Signal's link-and-sync history transfer, implemented by us on libsignal's `message-backup` crate and offered upstream to presage. Signal caps this at roughly 45 days; that ceiling is accepted and stated in the UI. Signal Desktop import is an opt-in extra, offered only when Desktop is detected, never required or suggested.
5. **Process model.** One always-on background agent per user per profile (macOS `SMAppService`, Windows logon task, Linux systemd user unit). It is the only process that talks to Signal, captures continuously, and serves a user-only local socket (Unix socket, or a named pipe on Windows). `signal-agent mcp` is a thin stdio front end that starts the agent if needed. At startup the agent re-adds any missing Claude registrations.
6. **MCP tools.** Parity with upstream signal-mcp's 81 tools is the v2 end state. v2.0 ships every tool presage can back, plus those we build on `send_message`. Tools that need a primary device are hidden on linked profiles.
7. **Encryption at rest.** SQLCipher for presage's store and the message store (with FTS5 search). The key lives in the Keychain, DPAPI, or the Secret Service; on headless Linux a 0600 key file, and setup says so.
8. **Profiles.** Each profile has its own account, store, key, agent, socket, and MCP server name (`signal-<profile>`). The first install makes one profile without mentioning the word; "Add another account" lives under Advanced.
9. **HTTP endpoint.** Off by default, under Advanced. MCP Streamable HTTP. Loopback unless the user opts into LAN. TLS required off loopback (user cert, or self-signed with a fingerprint shown for pinning). Per-client bearer tokens, scoped read-only or full, stored hashed, revocable, each request logged with the token name.
10. **Updates.** Daily check against GitHub Releases, native notification, installs only on click. Tauri update signature plus OS code signature.
11. **Uninstall.** In-app Uninstall, also run by the Windows uninstaller, removes the agent registration, the Claude entries, and Keychain or credential items. Messages and keys are deleted only when the user ticks a clearly labeled box.
12. **Testing.** Fake Signal behind one interface in CI on every push, with property tests at the tool registry (read-only enforcement, input validation) so every tool is covered automatically. A nightly link-and-message test between two accounts on Signal's staging servers. A real-account smoke checklist before every release.

## Anti-goals (v2.0)

1. No chat UI. The window does setup and settings only.
2. No mobile app; no Claude mobile support.
3. No cloud: no relay, hosted service, telemetry, or crash reporting.
4. No migration from v1, neither keys nor store. Relink; history returns through link-and-sync; v1 is removed with its own `uninstall.sh`.
5. No dependency on Signal Desktop, WSL, Docker, Java, Python, or Homebrew.
6. No silent updates.
7. No maintained fork of presage or libsignal-service-rs. Additions go upstream; we pin a git revision meanwhile.
8. Deferred to v2.x: group administration (create, update, join, leave, terminate), registration-lock PIN, usernames, account settings, and storage-service contact edits (block, unblock, update, remove). Added by the slice-0 audit (`docs/tool-audit.md`): `update_device` (presage doesn't expose device renaming), `send_story` (needs storage-service distribution lists), `upload_sticker_pack` (no upload endpoint at the pinned libsignal-service rev), and `start_change_number` / `finish_change_number` (no change-number flow). Two tools ship narrower than upstream in v2.0: `update_profile` without avatar or MobileCoin fields, and `set_expiration_timer` for direct messages only (a group timer is a group change). Any other tool presage can't back joins this list. The rule decides, not taste.
9. No OAuth on the HTTP endpoint.
10. No read-only default.
11. No Signal logo or Signal-blue speech bubble in the icon, and no "Signal"-first product name.

## Acceptance criteria (v2.0)

1. On a clean macOS 13 machine with no developer tools: download the `.dmg`, double-click, drag to Applications, open. No Gatekeeper warning; setup completes without a terminal ever appearing.
2. After in-app Uninstall on that machine, nothing from the app remains: no agent, no Claude entries, no Keychain items, and no data unless the user chose to keep it.
3. On clean Windows 11 x64 and ARM64: the `.exe` installs with no admin prompt and no restart. SmartScreen does not block once the publisher has reputation; until then a documented "More info, Run anyway" is acceptable.
4. On Ubuntu 22.04 over SSH on a headless box, the one-liner completes with the QR code in the terminal.
5. After linking, `search_messages` finds a message sent 30 days before linking, with Signal Desktop not installed.
6. Claude Desktop and Claude Code list the tools after the restart prompt; a send from each arrives on a phone.
7. A message received while no Claude app is open is findable later.
8. Claude Code and Claude Desktop running at the same time both send and receive with no errors and no session corruption across 100 interleaved operations.
9. Read-only mode hides every write tool and refuses them if called directly, enforced in the tool registry and checked by property tests.
10. The HTTP endpoint rejects missing and revoked tokens, refuses to listen off loopback without TLS, and a read-only token can't call a write tool.
11. A tag push with any signing secret missing fails the release job and publishes nothing.

## Open questions and risks

- Claude Desktop's custom connectors may require OAuth rather than a static bearer token, which would limit the HTTP endpoint to Claude Code and other clients.
- Link-and-sync is new protocol work presage lacks ([whisperfish/presage#396](https://github.com/whisperfish/presage/issues/396)); the ~45-day cap is unverified against Signal's docs. Riskiest slice; it goes first.
- Windows ARM64 depends on BoringSSL building for that target. If it doesn't, ARM64 slips to v2.1 rather than blocking v2.0.
- Signal server changes break unofficial clients without notice; the nightly staging test is the early warning.

## Prerequisites (owner, not code)

- Apple Developer Program, individual: Developer ID Application certificate and a notarization API key.
- Azure Artifact Signing account and certificate profile, verified as an individual.
- One or two spare phone numbers, registered once on Signal's staging servers.
- CI secrets set from those (`gh secret set`).

## Delivery order

0. Spike: link-and-sync with presage plus libsignal `message-backup`; audit all 81 tools against presage.
1. Tracer bullet: terminal link on macOS, background agent into an encrypted store, `signal-agent mcp` serving one read tool to Claude Code.
2. Socket protocol for concurrent clients.
3. Native setup on macOS (Tauri plus the setup state machine).
4. Signed macOS release (`.dmg`, notarization, release workflow).
5. Windows (native setup, NSIS installer, Artifact Signing, ARM64).
6. Linux (one-liner, systemd unit).
7. Remaining v2.0 tools in batches.
8. History: link-and-sync import, then optional Desktop import.
9. Profiles and own-number registration with the in-window captcha.
10. HTTP endpoint.
11. Updater and uninstall.
12. Nightly staging test and release checklist.
