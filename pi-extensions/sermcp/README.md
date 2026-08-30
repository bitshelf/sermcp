# pi.dev — sermcp Client Bridge

pi.dev deliberately ships **without built-in MCP support** (see
`docs/usage.md`: "It intentionally does not include built-in MCP ... You can
build or install those workflows as extensions or packages"). This extension
is the missing client side: it spawns the `sermcp` binary (stdio
mode), performs the MCP handshake, and registers every `serial_*` tool via
`pi.registerTool()` so pi can drive embedded DUTs exactly like Claude Code.

```
pi (agent)  ──►  pi.registerTool("serial_get_state") ──┐
                                                       │ stdio JSON-RPC
                       pi-extensions/sermcp │ (newline-delimited)
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

## Binary discovery

First hit wins:

1. `$SERMCP_BIN` — explicit override
2. project `.mcp.json` → `mcpServers["sermcp"].command` (with `${VAR}` expansion)
3. `PATH` lookup `sermcp`
4. `~/.local/bin/sermcp` — standard install location
5. project `bin/sermcp`
6. package-local `bin/sermcp` — plugin tarball layout
   (`scripts/package-pi-plugin.sh` puts the binary here)

If no binary is found, only `serial_mcp_diagnostics` is registered; the
agent can call it to see the discovery result and install hint.

## Tools

On `session_start` the extension decides between two roles:

- **Owner** (first session in a project): no server holds the project's
  control port, so the extension spawns the stdio server and registers every
  tool reported by `tools/list`. The server hands a stdio session its
  **agent-profile catalog**: the unified command entry `serial_exec`,
  state/logs/inventory queries, reset/power/button/U-Boot/flash tools, and
  `serial_claim` — 13 tools on a fully-configured DUT, fewer when the DUT
  lacks relay/button/RFC2217 config (the catalog is capability-gated).
  Implementation-detail tools (`serial_send_command`, learning/verify/baud
  tests, metrics/config queries) stay hidden; start pi with
  `SERMCP_TOOL_PROFILE=full` to see the complete catalog for debugging.
  `serial_mcp_diagnostics` is always registered.
- **Guest** (later sessions in the same project): the owner's control port
  (`md5(project_dir)[:8] % 99 + 3001`) is already serving, so the extension
  connects over Streamable HTTP (also agent profile) and registers only the
  read-only tools the server annotates (`readOnlyHint`):
  `serial_get_state`, `serial_list_duts`, `serial_get_logs`,
  `serial_poll_logs`. A guest never spawns a second stdio
  server — the project singleton lock would reject it — and never raises the
  `sermcp start failed` error for that expected case.

- Server `inputSchema` is passed straight through as the TypeBox
  `parameters` schema (every advertised schema verified to compile with
  TypeBox).
- `executionMode: "sequential"` — serial operations on a shared port must
  not race.
- MCP `isError` responses throw, so pi renders them as tool failures.
- If the connection dies, the next tool call re-resolves owner-vs-guest
  lazily (a former guest that finds the port free becomes the new owner).

## Lifecycle

| Event | Action |
| ------- | -------- |
| `session_start` | owner: resolve project dir + binary, spawn server, handshake, register tools; guest: connect read-only over HTTP |
| `session_shutdown` | close the stdio server or HTTP guest connection, reject in-flight requests |
| tool call (server dead) | respawn lazily with original binary/cwd |

Logs go to `.dut-serial/pi-mcp-client.log` in the project dir.

## Troubleshooting

```bash
# Is the bridge live? (returns binary, project dir, running flag, tool list)
pi -p "Call serial_mcp_diagnostics and report its output verbatim."

# Server-side log (Rust tracing)
tail -f .dut-serial/mcp.log

# Bridge-side log
tail -f .dut-serial/pi-mcp-client.log
```
