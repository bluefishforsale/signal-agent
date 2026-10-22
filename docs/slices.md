# Parallel delivery plan

How the PRD gets built by several agents at once without them colliding. The PRD says *what*; this file says *who builds which part, against which contract, in what order*. Integration checkpoints are the existing v2.0 issues (#1 to #13); lanes are the work that feeds them.

## Rules that make parallel work safe

1. **Contracts first, by one agent.** Wave 0 lands the whole workspace with every crate's public API, a fake Signal backend, stub implementations, and CI. After that, crate APIs change only through a small "contract change" PR that the integrating agent reviews and merges. Nobody edits another lane's API on the side.
2. **One lane owns each path.** A lane edits only the paths listed for it below. Needing a change elsewhere means opening an issue or a contract-change PR, not editing it.
3. **Worktree per agent, always.** Each agent works in its own `git worktree` on its own branch (`lane/<id>-<topic>`). `git fetch` before branching and before every push. Rebase onto the freshest `origin/main` only until the branch is first pushed; after that, bring base changes in with `git merge origin/main`. Never force-push.
4. **Green or it doesn't merge.** Every PR must pass `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` on macOS, Linux and Windows CI. Lanes test against `FakeSignal` and in-memory stubs, never a real Signal account.
5. **Vertical inside the lane.** Each PR is one test plus the smallest general implementation that passes it, following the repo owner's TDD rules. No speculative features beyond the lane's acceptance.
6. **Style.** Follow the Rust style guide given to every agent: two-tier errors (anyhow, plus `thiserror` only at a seam a caller matches on), no runtime unwraps, private modules with a curated `pub use` surface, a `//!` on each file, tests named for the invariant they pin, property tests at the registry.
7. **Authorship.** Commits and PRs carry the repo owner's authorship only: no `Co-Authored-By` trailer, no generated-by lines.

## Workspace and contracts (wave 0)

```
signal-agent/
  crates/
    relay-core      shared types: ProfileName, ProfileKind {Linked, Primary}, AccessMode {Full, ReadOnly},
                    Recipient {Contact(ServiceId), Group(GroupId)}, MessageId, Message, Conversation
    relay-signal    trait SignalBackend (link, register, receive stream, send, contacts, groups, attachments,
                    profile) + FakeSignal; the presage implementation lives here behind the trait
    relay-secrets   trait SecretStore (get/set/delete a profile's store key) with Keychain, DPAPI,
                    Secret Service and 0600-file implementations
    relay-store     SQLCipher message store with FTS5: open(path, key), insert (never overwrites), conversations, history,
                    search, unread, stats, prune, export
    relay-tools     trait Tool {name, description, input_schema, writes, needs_primary, call}, Registry,
                    ToolContext {store, signal, access, kind}; every MCP tool lives here
    relay-agent     the per-profile agent: owns SignalBackend + Store + Registry, speaks MCP JSON-RPC on a
                    user-only local socket (Unix socket / named pipe), login registration per OS
    relay-mcp       front ends onto the agent: stdio shim (connect-or-spawn, then copy bytes) and,
                    later, Streamable HTTP with tokens and TLS
    relay-setup     setup state machine (states and events, no UI), Claude Code and Claude Desktop
                    detection, registration, unregistration, Desktop restart
    relay-history   link-and-sync history import and opt-in Signal Desktop import, both into relay-store
  app/              Tauri v2 front end (macOS, Windows) over relay-setup
  bin/signal-agent  CLI: link, register, agent, mcp, setup (terminal front end), doctor, uninstall, http
  .github/workflows ci.yml (every push), release.yml (tags), nightly.yml (staging)
```

Two decisions that every lane depends on:

- The agent speaks MCP itself on its socket, so `signal-agent mcp` is just connect, start the agent if needed, then copy bytes between stdio and the socket. Tools execute in the agent, the only process holding the Signal connection (PRD goal 5). The HTTP front end serves the same MCP over HTTP. One protocol, three transports.
- One SQLCipher for the whole binary. presage-store-sqlite links `libsqlite3-sys` 0.36 with `bundled-sqlcipher` through `sqlx` 0.9, and presage's workspace patches `libsqlite3-sys` to whisperfish's fork. Our message store (L2) uses the same `sqlx` 0.9 and `libsqlite3-sys` 0.36, and the root `Cargo.toml` carries the same `[patch.crates-io]` entry as presage at the pinned revision (added by L1, the first lane to pull presage in). Two SQLite builds can't link into one binary, so nobody adds `rusqlite` or a second `libsqlite3-sys`. `MessageStore` is async for the same reason. Every crate that declares `libsqlite3-sys` uses exactly `features = ["bundled-sqlcipher-custom-crypto"]` with `sqlcipher-crypto-provider`, plus, under `[target.'cfg(windows)'.dependencies]`, `features = ["bundled-sqlcipher-vendored-openssl"]`, because at the pinned whisperfish revision SQLCipher's Windows build script requires OpenSSL (`OPENSSL_DIR`) even with custom crypto. Identical declarations everywhere keep the build independent of the dependency graph, since Cargo unifies features into one `libsqlite3-sys`. In practice that means RustCrypto on macOS and x86_64 Linux and OpenSSL on Windows and ARM Linux (the fork's custom-crypto code uses `*const i8` where ARM Linux's C `char` is unsigned, so it can't compile there; revert to custom crypto once the `c_char` fix lands upstream): when both features are on, the fork's build script checks vendored OpenSSL before custom crypto (L2's `sqlcipher_uses_the_declared_crypto_backend` test pins this with `PRAGMA cipher_provider`). Both write standard SQLCipher, so store files are portable. Don't fake `OPENSSL_DIR` to force custom crypto on Windows; that's a fragile trick against a build script. The windows-11-arm test job proves the vendored OpenSSL build on ARM64.
- Setup is a state machine in `relay-setup` with no UI code. The Tauri window and the Linux terminal flow are both thin renderers of its states, so setup behavior is tested once, without a UI.

## Lanes

| Lane | Owns | Builds | Feeds | Wave |
|---|---|---|---|---|
| L0 Contracts | everything, once | Workspace, all public APIs, FakeSignal, stubs, CI on 3 OSes, pre-commit fmt hook, unsigned hello-world Tauri build job | all | 0 |
| L1 Signal | `relay-signal` | presage implementation of `SignalBackend`: link, register (captcha and code inputs), receive, send to contacts and groups, contacts, groups, attachments, profile | #2, #10 | 1 |
| L2 Store and secrets | `relay-store`, `relay-secrets` | SQLCipher schema, FTS5 search, every store query tools need; all four secret stores, including DPAPI on Windows | #2, #6 | 1 |
| L3 Tools | `relay-tools` | Registry and the property test that covers every registered tool (read-only refusal, input validation, primary-only hiding); the 81-tool audit (`docs/tool-audit.md`); read tools first | #1, #2, #8 | 1, then batches |
| L4 Agent | `relay-agent`, `relay-mcp` (stdio), `bin/` except `bin/signal-agent/src/setup_tty.rs` | Agent process, socket with user-only permissions on all 3 OSes, connect-or-spawn shim, login registration (SMAppService, logon task, systemd user unit), startup re-registration hook | #2, #3, #6, #7 | 1 |
| L5 Setup | `relay-setup` | State machine, Claude client detection and registration (Code and Desktop on macOS and Windows; Code on Linux), Desktop restart, uninstall cleanup steps | #4, #6, #10, #12 | 1 |
| L6 Release | `.github/workflows/release.yml`, `app/` bundling config | `.dmg` (universal), NSIS x64 and ARM64, Linux tarballs and the one-liner, checksums; signing and notarization wired to the `release` environment, failing closed without secrets | #5, #6, #7 | 1 |
| L7 History | `relay-history` | Spike first (link-and-sync with libsignal `message-backup`), then production import, then opt-in Desktop import (key decoded as ASCII hex) | #1, #9 | 1 |
| L8 UI | `app/` (except bundling config) | Tauri screens rendering `relay-setup` states: account type, QR, access choice, history, done with Restart Claude Desktop; move-to-Applications; Advanced section | #4, #6, #10 | 2 |
| L9 Linux front end | `bin/signal-agent/src/setup_tty.rs`, `install.sh` one-liner | Terminal rendering of the same states, QR in terminal, captcha by paste, headless key-file warning | #7 | 2 |
| L3b to L3e Tool batches | one module each in `relay-tools` | Send and reply; reactions, edits and deletes; contacts, groups and profiles (read); local tools (export, prune, schedule, webhooks, stats). Batches touch separate files, so they run in parallel | #8 | 2 |
| L10 Profiles and registration | `relay-setup` profile module, `app/` registration screen | Named profiles end to end; own-number flow with in-window captcha capture | #10 | 2 |
| L11 HTTP | `relay-mcp` http module | Streamable HTTP, tokens (scoped, hashed, revocable, logged), TLS off loopback | #11 | 3 |
| L12 Updater and uninstall | `app/` updater module, `relay-setup` uninstall | Notify-then-click updater with Tauri signatures; complete uninstall with keep-data checkbox | #12 | 3 |
| L13 Staging | `.github/workflows/nightly.yml`, `tests/staging/` | Nightly two-account test on Signal staging, release checklist | #13 | 3 |

## Waves

- **Wave 0, 1 agent:** L0. Nothing else starts until it's merged, because every other lane builds against its contracts.
- **Wave 1, 7 agents:** L1 to L7 in parallel. Each works against `FakeSignal` and stubs, so none waits on another. L7's spike is the riskiest item in the PRD, which is why it starts this early.
- **Wave 2, up to 8 agents:** L8, L9, L10, and the L3b to L3e tool batches, starting once wave 1's APIs have real implementations behind them.
- **Wave 3, 3 agents:** L11, L12, L13.

The integrating agent merges lane PRs and closes checkpoint issues when their acceptance criteria pass. It's also the only one that touches cross-lane contracts.

## Agent brief (paste into each lane's agent)

```
You are lane <Lx> of signal-agent (docs/PRD.md, docs/slices.md).
Worktree: ~/Projects/bluefishforsale/signal-agent-<lx>, branch lane/<lx>-<topic>, off fresh origin/main.
You own: <paths>. Do not edit anything else; open an issue for a needed contract change.
Build against: relay-core types, the trait APIs as merged in L0, FakeSignal for tests.
Deliver: <lane acceptance from its issue>. One failing test, then the smallest general code that
passes it; repeat. Follow the Rust style guide you were given.
Gates before every push: cargo fmt -- --check; cargo clippy --all-targets -- -D warnings; cargo test.
Before the first push, rebase on origin/main; after it, merge origin/main in instead (never force-push).
Never search ~ or ~/Library (find/fd/rg/ls -R): it triggers macOS privacy prompts; use your worktree or exact paths.
Open a PR into main; no Co-Authored-By or generated-by lines. Never merge PRs or delete branches.
Report: what merged, which acceptance boxes are checked, anything blocked and on whom.
```
