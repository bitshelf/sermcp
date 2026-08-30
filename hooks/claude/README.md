# Embedded Debug Hooks for Claude Code

Serial console monitoring hooks — auto-start MCP server + real-time statusline.

## Files

| Hook | Trigger | Function |
|------|---------|----------|
| `session-start.py` | Session start | Detect `.target.jsonc` → `dutabo list --json` bootstrap → one project MCP |
| `statusline.py` | 1s refresh | Render `.dut-serial/inventory.json` `state_text` verbatim (never execs) |
| `pre-tool-use.py` | Before Bash | Intercept raw serial access → prompt MCP (fail-closed without inventory) |
| `user-prompt-submit.py` | Before prompt | Alert on DUT-off/disconnected/crashed |
| `session-stop.py` | Session stop | Kill saved session PID, clean sentinels + locks |
| `lib.py` | Shared | find_project_dir, check_mcp_alive |
| `inventory.py` | Shared | load_inventory, find_project_root, render_state |
| `owner.py` | Shared | project ownership record (first-open-wins, MCP-pid liveness anchor) |

Hooks never parse config in any dialect. The Rust server writes
`.dut-serial/inventory.json` (at startup and on every state transition);
`session-start.py` bootstraps it once per session via `dutabo list --json`
(ports/dev hosts come from Rust only). Latency-critical hooks (statusline,
pre-tool-use) read inventory + one `os.kill(mcp_pid, 0)` freshness check and
must never exec a subprocess (test-enforced).

## Deploy

```bash
cp hooks/claude/*.py ~/.claude/hooks/embedded-debug/
```

The statusline selects the Rust-generated owner or guest view by session ID.
`dutabo` (interactive CLI holds the port) applies to every session; a guest
session shows `busy` — the MCP is held by another agent session and this one
is read-only. Occupancy is the state that session acts on, not a cosmetic
mask: `serial_get_state` reports it as the primary `state` with the device
state alongside in `device_state`, and the inventory keeps the raw device
states.
