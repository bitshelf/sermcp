# Codex integration

Codex can use `sermcp` directly over stdio and can receive the DUT
state as hook context. Codex's built-in `/statusline` picker does not currently
accept arbitrary external-command fields, so the live human-facing view uses
the integrated terminal instead of claiming to add a native footer item.

From the embedded target project (the directory containing `.target.jsonc`
or `.dut-serial/inventory.json`):

```bash
mkdir -p .codex/hooks
cp /path/to/sermcp/hooks/codex/config.toml.example .codex/config.toml
cp /path/to/sermcp/hooks/codex/hooks.json .codex/hooks.json
cp /path/to/sermcp/hooks/codex/serial-status.py .codex/hooks/
codex mcp list
```

Trust the project and review the new hooks with `/hooks`, then restart Codex.
The MCP tools `serial_get_state`, `serial_poll_logs`, and
`serial_get_logs` are then available to the agent. `SessionStart` and
`UserPromptSubmit` inject the current DUT state into Codex context; critical
states also appear as a UI warning.

For an event-driven display of one DUT, keep this running in Codex's integrated
terminal. This path uses the Rust `notify` crate (inotify on Linux), without
polling:

```bash
dutabo status --watch --dut <board_name>
```

For a compact multi-DUT display, the Python helper remains available (it polls
the Rust-written `.dut-serial/inventory.json` every 0.5 seconds):

```bash
python3 .codex/hooks/serial-status.py --watch
```

This only reads `.dut-serial/inventory.json` (state pre-formatted by the Rust
server); it does not claim the serial port and does not interfere with MCP or
`dutabo serial`.

Session-aware display uses the hook payload's `session_id`, falling back to
`CODEX_THREAD_ID`. For a separate terminal, pass `--session-id <id>` to the
Python helper. Hooks share the project ownership record with Claude; a later
session is a guest and selects the server-rendered `busy` view — the MCP is
held by another agent session and the guest's tools are read-only. During
`dutabo serial`, both owner and guest views show `dutabo`. Occupancy is the
state that session acts on (also the primary `state` of `serial_get_state`,
with the device state in `device_state`); raw device states remain in the
inventory. Use `--release --session-id <id>` when explicitly ending a
hook-managed session.
