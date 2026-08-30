# pi.dev — sermcp MCP integration

pi >= 1.0 ships built-in MCP support. This extension does not speak MCP
itself: it discovers the project and the `sermcp` binary, registers the
server with `pi.registerMcpServer()`, and keeps one diagnostics tool.

```text
pi (agent) ──► built-in MCP client ──► mcp__sermcp__serial_exec
                       │ stdio JSON-RPC
                       ▼
              sermcp binary (Rust MCP server)
                       │
              ser2net (TCP) ──► DUT serial console
```

## Install

The repo root is a pi package (`package.json` `pi` manifest). From the repo
root:

```bash
pi install .
# or, from anywhere: pi install /path/to/sermcp
```

The extension is loaded from the package manifest
(`pi.extensions: ["./pi-extensions/sermcp"]`). Restart pi or run
`/reload` after installing.

Alternatively, configure the server yourself in `~/.pi/agent/mcp.json` or
`.pi/mcp.json`; a file entry with the name `sermcp` takes precedence over
this extension's registration:

```json
{
  "mcpServers": {
    "sermcp": {
      "command": "sermcp",
      "env": { "TARGET_CONF": "/abs/path/.target.jsonc" },
      "exposure": "direct",
      "timeout": 1800,
      "description": "Serial console for embedded DUTs"
    }
  }
}
```

## Binary discovery

First hit wins:

1. `$SERMCP_BIN` — explicit override
2. project `.mcp.json` / `.pi/mcp.json` → `mcpServers["sermcp"].command`
   (with `${VAR}` expansion)
3. `PATH` lookup `sermcp`
4. `~/.local/bin/sermcp` — standard install location
5. project `bin/sermcp`
6. package-local `bin/sermcp` — plugin tarball layout
   (`scripts/package-pi-plugin.sh` puts the binary here)

If no binary is found in a configured project (`$TARGET_CONF` or an
`.target.jsonc` in the directory tree), the session shows a warning and
`serial_mcp_diagnostics` reports the discovery result and install hint.

## Registration

On extension load (and again on `session_start`), the extension registers:

- server name `sermcp`
- `command` — the discovered binary, `cwd` — the project directory
- `env.TARGET_CONF` — the resolved `.target.jsonc`
- `exposure: "direct"` — tools are declared to the model as
  `mcp__sermcp__serial_*`; no tool-search or codemode step
- `timeout: 1800` — a client-side backstop only; the server owns the real
  per-tool deadlines (`serial_reset(wait_boot=true)` waits across retries;
  the default worst case is ≈545s). A call that passes a larger explicit
  timeout (`serial_exec.timeout`, `serial_reset.failure_retry`) keeps
  running on the server after pi gives up on the request.

The server exposes the capability-gated agent catalog (13 tools on a
fully-configured DUT, fewer without relay/button/RFC2217 config). The
capability filtering and the read-only guest role are enforced by the Rust
server — pi's built-in client is transport only.

`serial_mcp_diagnostics` is an extension tool, not an MCP tool, so it also
works when the server is not connected.

## Multiple sessions in one project

The Rust server keeps one engine per project. The first stdio client owns
it; a later stdio client is transparently bridged into the running instance
over its loopback HTTP endpoint and served read-only. Every pi session can
therefore register the same stdio command — no owner/guest bookkeeping in
the extension, and no `sermcp start failed` error for the expected case.

## Troubleshooting

```bash
# Inspect the live connection and every tool pi sees
#   (interactive session) /mcp
#   (agent) Call serial_mcp_diagnostics and report its output verbatim.

# Server-side log (Rust tracing)
tail -f .dut-serial/mcp.log

# Extension-side log (registration decisions)
tail -f .dut-serial/pi-extension.log
```

`pi mcp list` from a shell does not load extensions, so an
extension-registered server does not appear there. Use `/mcp` inside a
session.
