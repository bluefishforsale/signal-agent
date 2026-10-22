# Uninstall checklist

Generated from `crates/relay-setup/src/uninstall/footprint.rs`, the list uninstall itself works through; a test fails if this file and the code disagree. Regenerate with `UPDATE_UNINSTALL_CHECKLIST=1 cargo test -p relay-setup uninstall_checklist`.

For PRD acceptance 2, after Uninstall every row marked *always* must be gone. Rows marked *if ticked* are gone only when "Also delete my messages, account data and the keys that unlock them" was ticked, and must otherwise still be there. Rows marked *by you* are outside what an app may remove. Rows are shown for the default profile `personal`; every other profile adds the same per-profile rows under its own name.

## macOS

| # | What | Where | Removed |
|---|---|---|---|
| 1 | File or folder | `~/Library/Application Support/signal-agent/profiles.json` | always |
| 2 | Login item | `launch agent com.bluefishforsale.signalagent.agent, listed in System Settings > General > Login Items` | always |
| 3 | Claude Code entry | `user-scope MCP server signal-personal in ~/.claude.json` | always |
| 4 | Claude Desktop entry | `mcpServers.signal-personal in ~/Library/Application Support/Claude/claude_desktop_config.json` | always |
| 5 | File or folder | `~/Library/Application Support/Claude/claude_desktop_config.json.bak-signal-agent` | always |
| 6 | File or folder | `~/Library/Application Support/Claude/claude_desktop_config.json.tmp-signal-agent` | always |
| 7 | Running agents | `every signal-agent process of this user` | always |
| 8 | Database key | `login Keychain generic password, service signal-agent, account personal` | if ticked |
| 9 | File or folder | `~/Library/Application Support/signal-agent/personal/agent.log` | always |
| 10 | File or folder | `~/Library/Application Support/signal-agent/personal/agent.sock` | always |
| 11 | File or folder | `~/Library/Application Support/signal-agent/update-check.json` | always |
| 12 | File or folder | `~/Library/Application Support/signal-agent/login-item-supervisor` | always |
| 13 | File or folder | `~/Library/Application Support/com.bluefishforsale.signalagent` | always |
| 14 | File or folder | `~/Library/Caches/com.bluefishforsale.signalagent` | always |
| 15 | File or folder | `~/Library/WebKit/com.bluefishforsale.signalagent` | always |
| 16 | File or folder | `~/Library/HTTPStorages/com.bluefishforsale.signalagent` | always |
| 17 | File or folder | `~/Library/Saved Application State/com.bluefishforsale.signalagent.savedState` | always |
| 18 | File or folder | `~/Library/Application Support/signal-agent` | if ticked |
| 19 | Left for you | The app itself: drag MCP Relay for Signal from Applications to the Trash | by you |

## Windows

| # | What | Where | Removed |
|---|---|---|---|
| 1 | File or folder | `%LOCALAPPDATA%\signal-agent\profiles.json` | always |
| 2 | Logon task | `Task Scheduler task signal-agent, and any signal-agent-<profile> task from an older version` | always |
| 3 | Claude Code entry | `user-scope MCP server signal-personal in %USERPROFILE%\.claude.json` | always |
| 4 | Claude Desktop entry | `mcpServers.signal-personal in %APPDATA%\Claude\claude_desktop_config.json` | always |
| 5 | File or folder | `%APPDATA%\Claude\claude_desktop_config.json.bak-signal-agent` | always |
| 6 | File or folder | `%APPDATA%\Claude\claude_desktop_config.json.tmp-signal-agent` | always |
| 7 | Running agents | `every signal-agent process of this user` | always |
| 8 | Database key | `DPAPI-protected key file in the personal profile folder` | if ticked |
| 9 | File or folder | `%LOCALAPPDATA%\signal-agent\personal\agent.log` | always |
| 10 | File or folder | `%LOCALAPPDATA%\signal-agent\update-check.json` | always |
| 11 | File or folder | `%LOCALAPPDATA%\com.bluefishforsale.signalagent` | always |
| 12 | File or folder | `%APPDATA%\com.bluefishforsale.signalagent` | always |
| 13 | File or folder | `%LOCALAPPDATA%\signal-agent` | if ticked |
| 14 | Left for you | The app itself: Settings > Apps > MCP Relay for Signal > Uninstall, which runs this same cleanup first | by you |

## Linux

| # | What | Where | Removed |
|---|---|---|---|
| 1 | File or folder | `~/.local/share/signal-agent/profiles.json` | always |
| 2 | systemd user unit | `~/.config/systemd/user/signal-agent.service, and any signal-agent-<profile>.service from an older version` | always |
| 3 | Claude Code entry | `user-scope MCP server signal-personal in ~/.claude.json` | always |
| 4 | Running agents | `every signal-agent process of this user` | always |
| 5 | Database key | `Secret Service item (service signal-agent, profile personal), or the 0600 key file in the profile folder on a machine without one` | if ticked |
| 6 | File or folder | `~/.local/share/signal-agent/personal/agent.log` | always |
| 7 | File or folder | `~/.local/share/signal-agent/personal/agent.sock` | always |
| 8 | File or folder | `~/.local/share/signal-agent` | if ticked |
| 9 | Left for you | The signal-agent program itself, e.g. ~/.local/bin/signal-agent | by you |
| 10 | Left for you | User lingering, which setup turned on so the agent runs after logout: `loginctl disable-linger` if nothing else needs it | by you |
