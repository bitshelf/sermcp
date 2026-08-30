#!/usr/bin/env python3
"""
UserPromptSubmit hook — alert agent if target serial state needs attention.

Reads the Rust-written .dut-serial/inventory.json (schema_version 1),
outputs {"continue": true} or {"systemMessage": "..."}.

Data source: per-DUT `state` + `critical` come from the inventory. The
ser2net reachability check keeps its TCP probe but takes host/port from
the inventory (dev_host_ip + serial_port), and the MCP liveness gate is
the inventory mcp_pid via os.kill.
"""

import json
import sys
from pathlib import Path

_HOOK_DIR = Path(__file__).resolve().parent
if str(_HOOK_DIR) not in sys.path:
    sys.path.insert(0, str(_HOOK_DIR))

from lib import (
    agent_session_process,
    check_mcp_alive,
    detect_endpoint_contention,
    find_project_dir,
    http_health_ok,
    project_locked,
    read_hook_payload,
    spawn_http_mcp,
    resolve_plugin_binary,
)
from inventory import load_inventory
from owner import load_owner, am_i_owner, owner_is_alive


def _mcp_http_port(project_dir: str):
    """The MCP HTTP port for this project — ONLY from the Rust-written
    inventory.json (`mcp_http_port`, the actual bound --http port).

    No hardcoded fallback: in stdio mode `.mcp.json` has no `url`, and the
    old default of 3000 would spawn a server on the WRONG port (every
    project's port is derived from its path). No inventory / no port →
    None → the caller reports and does NOT spawn anything."""
    inv = load_inventory(project_dir)
    if inv is None:
        return None
    port = inv.get("mcp_http_port")
    if isinstance(port, int) and 0 < port <= 65535:
        return port
    return None


def _check_ser2net(host: str, port) -> bool:
    """TCP connectivity check — ser2net reachable?"""
    import socket
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(2)  # 2s (was 3s — reduce user wait)
        s.connect((host, int(port)))
        s.close()
        return True
    except (ValueError, ConnectionRefusedError, OSError):
        return False


def _restart_mcp(project_dir: str) -> None:
    """Restart an unowned HTTP project on its recorded port."""
    if (Path(project_dir) / ".dut-serial" / "stopped").exists():
        return

    try:
        config = json.loads((Path(project_dir) / ".mcp.json").read_text())
        if config.get("mcpServers", {}).get("sermcp", {}).get("command"):
            print("[sermcp] stdio server is managed by the Agent host; reconnect its MCP transport.", file=sys.stderr)
            return
    except (OSError, ValueError, TypeError):
        pass
    mcp_port = _mcp_http_port(project_dir)
    if mcp_port is None:
        print(
            "[sermcp] cannot restart the MCP: no mcp_http_port in "
            ".dut-serial/inventory.json (run any MCP tool or `dutabo list` "
            "first) — not spawning.",
            file=sys.stderr,
        )
        return

    # Healthy → do nothing (never kill a working server).
    if project_locked(project_dir) or http_health_ok(mcp_port, timeout=2.0):
        return

    if agent_session_process() is None:
        print("[sermcp] Cannot bind HTTP restart to an Agent session; use stdio. Existing processes are untouched.", file=sys.stderr)
        return

    binary = resolve_plugin_binary("sermcp")
    if not binary:
        print(
            "[sermcp] cannot restart the MCP: sermcp "
            "binary not found — not spawning.",
            file=sys.stderr,
        )
        return

    spawn_http_mcp(project_dir, mcp_port, binary=binary)


def _critical_alerts(duts: list) -> list:
    """Per-DUT critical alerts from the inventory: "[TARGET-ALERT] <dut_name>
    is <state>". `critical` is set by the shared Rust classifier — true
    only for crashed / DUT-off / disconnected."""
    return [
        f"[TARGET-ALERT] {d['dut_name']} is {d['state']}"
        for d in duts
        if d.get("critical") and d.get("state") in ("crashed", "DUT-off", "disconnected")
    ]


def main():
    # Session identity for ownership gating (multi-agent isolation).
    payload = read_hook_payload()
    session_id = payload.get("session_id")

    # Display path: no /dev/shm cross-project fallback — an agent must never
    # alert about (or restart the MCP of) another project.
    cwd = payload.get("cwd")
    project_dir = find_project_dir(allow_lock_fallback=False, start=Path(cwd) if isinstance(cwd, str) and cwd else None)
    if not project_dir:
        print(json.dumps({"continue": True}))
        sys.exit(0)

    # A non-owner NEVER restarts the MCP (multi-agent isolation): alert
    # only. Ownership belongs to the first agent (the owner record), with
    # liveness anchored on owner_mcp_pid.
    owner = load_owner(project_dir)
    may_restart = am_i_owner(owner, session_id) or owner is None

    inv = load_inventory(project_dir)
    if inv is None:
        # No inventory at all: no server ever wrote one.
        print(json.dumps({
            "systemMessage": (
                "[TARGET] MCP serial server is not running. "
                "Call any MCP tool (e.g. serial_get_state) to start it."
            )
        }))
        sys.exit(0)

    duts = inv.get("duts", [])

    # PID liveness: a dead server's inventory is stale. If it died while
    # disconnected, keep the recovery path (ser2net probe + restart).
    if not check_mcp_alive(project_dir):
        dead_disconnected = [d for d in duts if d.get("state") == "disconnected"]
        if dead_disconnected:
            # One probe per DISTINCT (dev_host, port): N DUTs behind the
            # same ser2net must not cost N × 2s of prompt latency.
            reachable = all(
                _check_ser2net(host, port)
                for host, port in {
                    (d.get("dev_host_ip", ""), d.get("serial_port", ""))
                    for d in dead_disconnected
                    if d.get("dev_host_ip") and d.get("serial_port")
                }
            )
            if reachable:
                # ser2net OK → problem is local MCP → auto-restart (owner only)
                if may_restart:
                    _restart_mcp(project_dir)
                    print(json.dumps({
                        "systemMessage": "[TARGET] MCP server is down; recovery was requested. Reconnect the host-managed MCP transport if it remains unavailable."
                    }))
                else:
                    print(json.dumps({
                        "systemMessage": (
                            "[TARGET-ALERT] MCP serial server is down but this "
                            f"session is not the owner (owner session "
                            f"{owner.get('owner_session_id') if owner else '?'}); "
                            "the owner agent will restart it."
                        )
                    }))
            else:
                print(json.dumps({
                    "systemMessage": (
                        "[TARGET-ALERT] Serial connection lost — "
                        "ser2net on dev host unreachable."
                    )
                }))
            sys.exit(0)
        print(json.dumps({
            "systemMessage": (
                "[TARGET] MCP serial server is not running. "
                "Call any MCP tool (e.g. serial_get_state) to start it."
            )
        }))
        sys.exit(0)

    # ── Live server: alert on critical states from the inventory ─────────
    alerts = _critical_alerts(duts)

    # Audit even a healthy inventory; socket conflicts require attention.
    contention = detect_endpoint_contention(project_dir)

    if alerts:
        crashed = any(d.get("state") == "crashed" for d in duts)
        dut_off = any(d.get("state") == "DUT-off" for d in duts)
        disconnected = [d for d in duts if d.get("state") == "disconnected"]
        if disconnected:
            # One probe per DISTINCT (dev_host, port) — see above.
            reachable = all(
                _check_ser2net(host, port)
                for host, port in {
                    (d.get("dev_host_ip", ""), d.get("serial_port", ""))
                    for d in disconnected
                    if d.get("dev_host_ip") and d.get("serial_port")
                }
            )
            if reachable:
                # ser2net OK → problem is local MCP → auto-restart (owner only)
                if may_restart:
                    _restart_mcp(project_dir)
                    alerts.append("[TARGET] ser2net OK, MCP reconnected.")
                else:
                    alerts.append(
                        "[TARGET-ALERT] MCP needs a restart but this session "
                        "is not the owner; the owner agent will restart it."
                    )
            else:
                alerts.append(
                    "[TARGET-ALERT] Serial connection lost — "
                    "ser2net on dev host unreachable."
                )
        elif crashed:
            alerts.append(
                "Kernel crash detected! "
                'Run serial_get_logs(pattern="panic|BUG|Oops|Call trace") to see details.'
            )
        elif dut_off:
            alerts.append(
                "DUT-off — no serial output for extended period. "
                'Try serial_exec("echo ping") or serial_reset().'
            )
        alerts.extend(contention)
        print(json.dumps({"systemMessage": " | ".join(alerts)}))
        sys.exit(0)

    if contention:
        print(json.dumps({"systemMessage": " | ".join(contention)}))
        sys.exit(0)

    print(json.dumps({"continue": True}))


if __name__ == "__main__":
    main()
