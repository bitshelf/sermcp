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
    _is_embedded_server,
    check_mcp_alive,
    find_project_dir,
    http_health_ok,
    pid_anchored_to_project,
    read_hook_payload,
    spawn_http_mcp,
    resolve_plugin_binary,
    terminate_pid,
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
    """Restart the project's HTTP MCP server — health-check FIRST.

    Order matters:
      1. resolve the port from inventory.json (`mcp_http_port`) — when no
         port is known, print a notice and spawn NOTHING (a wrong-port
         spawn fights the real server for the serial lock);
      2. GET /health (2s) — a healthy server is NEVER touched, so this is
         idempotent and quiet when everything is fine;
      3. targeted kill: only processes verified as sermcp that
         still hold the port (a wedged server blocking the bind);
      4. respawn via the shared lib.spawn_http_mcp — the same argv/env as
         session-start.py, with TARGET_CONF pinned to THIS project.
    """
    import subprocess

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
    if http_health_ok(mcp_port, timeout=2.0):
        return

    binary = resolve_plugin_binary("sermcp")
    if not binary:
        print(
            "[sermcp] cannot restart the MCP: sermcp "
            "binary not found — not spawning.",
            file=sys.stderr,
        )
        return

    # Targeted kill: only sermcp processes that hold the MCP
    # port AND are anchored to THIS project. The port range (3001–3099) is
    # narrow and hash-derived, so two projects CAN collide — without the
    # anchoring check, a prompt here could SIGTERM another project's
    # wedged-but-recoverable server.
    try:
        result = subprocess.run(
            ["fuser", f"{mcp_port}/tcp"], capture_output=True, text=True, timeout=5
        )
        if result.returncode == 0 and result.stdout.strip():
            for pid_str in result.stdout.strip().split():
                try:
                    pid = int(pid_str)
                    if (
                        _is_embedded_server(pid)
                        and pid_anchored_to_project(pid, project_dir)
                    ):
                        terminate_pid(
                            pid,
                            grace_secs=3.0,
                            identity_check=lambda p: _is_embedded_server(p)
                            and pid_anchored_to_project(p, project_dir),
                        )
                except (ValueError, OSError, ProcessLookupError):
                    pass
    except (subprocess.TimeoutExpired, OSError):
        pass

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
    session_id = read_hook_payload().get("session_id")

    # Display path: no /dev/shm cross-project fallback — an agent must never
    # alert about (or restart the MCP of) another project.
    project_dir = find_project_dir(allow_lock_fallback=False)
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
                        "systemMessage": "[TARGET] ser2net OK, MCP reconnected."
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
                'Try serial_send_command("echo ping") or serial_reset().'
            )
        print(json.dumps({"systemMessage": " | ".join(alerts)}))
        sys.exit(0)

    print(json.dumps({"continue": True}))


if __name__ == "__main__":
    main()
