# HTTP endpoint (Advanced)

The HTTP endpoint lets an MCP client on another machine reach a profile over MCP Streamable HTTP (PRD goal 9, issue #11). It's off unless you turn it on, it listens on loopback unless you opt into LAN, and off loopback it only ever speaks TLS. Every request needs a bearer token you made for that client, and no MCP tool can change any of this: the controls are the `signal-agent http` command (and later the app's Advanced screen), so a prompt-injected client can't widen its own access.

## How it fits

The endpoint runs inside the `signal-agent agent` process when it's enabled, but it reaches Signal the same way the stdio shim does: as a client of the agent's user-only local socket. Each HTTP request is replayed on a fresh connection to the agent, so the agent's registry is the only place access is decided. A read-only token's connection declares the `signal-agent/read-only` capability at initialize, and the agent serves that connection read-only (a client can lower its access this way, never raise it). A full token gets whatever the profile allows, so a full token on a read-only profile is still read-only. The server runs in MCP's stateless mode with JSON responses, so there are no sessions to hijack between tokens. To keep one client, or a flood of them, from taking the endpoint down, it serves at most 64 connections at once (the rest wait in the backlog), drops a connection that hasn't finished its TLS handshake within 10 seconds or its request headers within 30, and pauses and keeps going after transient accept errors such as running out of file descriptors.

## Turning it on

```sh
signal-agent http enable                      # http://127.0.0.1:8765/mcp, this machine only
signal-agent http enable --lan --tls-name mybox.local --tls-name 192.168.1.20
signal-agent http enable --lan --cert fullchain.pem --key privkey.pem
signal-agent http status
signal-agent http disable
```

Settings live in the profile directory as `http.json` and apply the next time the agent starts. With `--lan` and no certificate of your own, the endpoint makes a self-signed one (`http-cert.pem` and `http-key.pem`, both mode 0600) valid for `localhost` plus every `--tls-name`, keeps it across restarts, and prints its SHA-256 fingerprint in `http status` and the agent log so a client can pin it. To make a new one, for example with different names, delete those two files and run `http status` again. Asking for an address that isn't loopback without TLS is refused before anything binds.

## Tokens

```sh
signal-agent http token create laptop --scope read-only
signal-agent http token create desktop --scope full
signal-agent http token list
signal-agent http token revoke laptop
```

A token is 32 random bytes, printed once at creation and never again. The profile keeps only its SHA-256 in `http_tokens.json` (mode 0600, replaced whole by write-and-rename like `http.json`), compares hashes in constant time, and rereads the file on every request, so a revoked token fails on its very next request without restarting anything. Names must be unique, since they're how you revoke a client and how the log identifies it: every request is logged with the token's name, the peer address, the method, path and status, and never the token. Requests with a missing or unknown token get `401` with `WWW-Authenticate: Bearer`.

## Claude Code

```sh
claude mcp add --transport http signal https://mybox.local:8765/mcp \
  --header "Authorization: Bearer <token>"
```

With a self-signed certificate, Claude Code (Node) has to trust it: point `NODE_EXTRA_CA_CERTS` at a copy of `http-cert.pem`, and dial one of the names the certificate was made for. Checking this end to end from a second machine is the owner's acceptance step; the automated tests cover loopback only.

## Claude Desktop connectors

This answers the PRD's open question, from Anthropic's connector docs as of October 2026 ([Authentication for connectors](https://claude.com/docs/connectors/building/authentication)), not verified by hand. Custom connectors in Claude Desktop, claude.ai and mobile do support a static bearer token, through a "Request headers" section when adding the connector, but that is a beta limited to some organizations and only an organization Owner can enter it; everyone else gets OAuth or no authentication, and this endpoint has no OAuth by design (anti-goal 9). The bigger limit is network: connector requests come from Anthropic's servers (`160.79.104.0/21`), not from your computer, so a loopback or LAN endpoint is unreachable from a Desktop connector whatever the auth. Claude Desktop should keep using the local stdio registration setup already writes; the HTTP endpoint is for Claude Code and other MCP clients that connect from your own machines.
