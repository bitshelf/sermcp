#!/usr/bin/env python3
"""Shared inventory consumption for the embedded-debug hooks — stdlib only.

Python parses ZERO config. The single data source is the Rust-written
`.dut-serial/inventory.json`
(schema_version 1), written atomically by the MCP server at startup, on
every state transition, AND on a 30s heartbeat (the server
refreshes `written_at_unix` even when idle, so freshness is a trustworthy
liveness signal) — and emitted identically by `dutabo list --json`. It
carries pre-formatted ANSI `state_text` per DUT — hooks render it VERBATIM
and never map state -> color themselves (that mapping lives in
`state_manager.rs` and cannot drift).

Latency contract: every function here is file reads + os.kill only — no
subprocess, no network — so the statusline/pre-tool-use hot path stays
well under the 2ms budget.

Liveness gate (`inventory_is_live`): an inventory
renders ONLY when BOTH hold —
  1. the recorded mcp_pid is alive AND is actually a sermcp
     process (comm or argv[0] basename via /proc — a recycled pid must
     never make a stale snapshot look live); AND
  2. when the server declared a heartbeat (`heartbeat_secs`), written_at
     is fresh (≤ 3 × heartbeat). A killed server leaves no cleanup hook,
     so without freshness the old state would render forever; the
     heartbeat contract makes written_at reliable.
  A snapshot failing either check renders NOTHING — never a made-up state.

Schema (mirrors `config::InventoryJson`, schema_version 1):
  schema_version: 1
  written_by: "mcp-server" | "dutabo"
  written_at_unix: <seconds>
  mcp_pid: <int> | null        # liveness signal — hooks trust this, not time
  mcp_http_port: <int> | null
  heartbeat_secs: <int> | null # server's inventory rewrite interval
  current_dut: <dut_name> | null
  dev_hosts: [{ip, user, host_name}]
  duts: [{dut_name, dev_host_ip, ssh_user, serial_port,
          dut_dir, mcp_port, state, state_label, state_text, state_plain,
          age_secs: <int>|null, critical: <bool>}]
"""

from __future__ import annotations

import glob
import json
import os
from pathlib import Path
from typing import Optional

INVENTORY_FILE = "inventory.json"
CONFIG_FILE = ".target.jsonc"

# Directories that must NEVER be treated as project roots themselves, even
# if a config file was left there by a previous session.
_FORBIDDEN_ROOTS = {"/tmp", "/var/tmp", "/dev/shm", "/run", "/proc", "/sys", "/dev"}


def _get_excluded_paths() -> set[Path]:
    """Read TARGET_EXCLUDE env var (colon-separated absolute paths)."""
    raw = os.environ.get("TARGET_EXCLUDE", "")
    if not raw:
        return set()
    excluded = set()
    for part in raw.split(":"):
        part = part.strip()
        if part:
            excluded.add(Path(part).expanduser().resolve())
    return excluded


def _is_excluded(d: Path) -> bool:
    """Check if a directory is in the exclusion list."""
    resolved = d.resolve()
    for ex in _get_excluded_paths():
        if resolved == ex or str(resolved).startswith(str(ex) + os.sep):
            return True
    return False


def _read_lock(lock_path: str) -> Optional[str]:
    """Read a /dev/shm lock file, return content if a valid directory path."""
    p = Path(lock_path)
    if p.exists():
        try:
            content = p.read_text().strip()
            if content and Path(content).is_dir():
                return content
        except OSError:
            pass
    return None


def _check_dir(d: Path) -> Optional[str]:
    """Classify one directory: the project dir, or None.

    Rejects the forbidden roots themselves and TARGET_EXCLUDE'd dirs
    (subdirectories of the forbidden roots are fine). Strongest signal
    first: a live inventory (Rust-written) beats a bare `.target.jsonc`
    (config present, server not started yet).
    """
    resolved = str(d.resolve())
    if resolved in _FORBIDDEN_ROOTS or _is_excluded(d):
        return None
    if (d / ".dut-serial" / INVENTORY_FILE).is_file():
        return str(d)
    if (d / CONFIG_FILE).is_file():
        return str(d)
    return None


def find_project_root(
    allow_lock_fallback: bool = True,
    start: Optional[Path] = None,
) -> Optional[str]:
    """Find the project root; returns the project dir or None.

    One shared discovery for every hook (lib.find_project_dir,
    session-start.find_projects, statusline and codex all route through
    here). `start` overrides the walk-up origin (the hook payload's cwd —
    resolution must never depend on the process CWD). Order:
      1. TARGET_CONF env (explicit override) — a set-but-missing path
         yields None: the hooks stay silent (the Rust server errors
         loudly instead);
      2. walk-up from `start` (default: process CWD) for
         `.dut-serial/inventory.json` (live project), then `.target.jsonc`;
      3. /dev/shm claude-serial-*.lock fallback (projects from other
         sessions whose CWD walk-up cannot see them) — ONLY when
         `allow_lock_fallback` is true. The fallback matches ANY lock, so
         with several projects active it is arbitrary cross-project
         leakage: display paths (statusline) must pass False and render
         nothing instead of another project's DUT state. Session-lifecycle
         paths (server keep-alive) keep the default True.
    """
    if env := os.environ.get("TARGET_CONF"):
        p = Path(env)
        if p.exists():
            d = p.resolve().parent
            if not _is_excluded(d) and str(d.resolve()) not in _FORBIDDEN_ROOTS:
                return str(d)
        return None

    d = Path(start).resolve() if start else Path.cwd()
    while True:
        found = _check_dir(d)
        if found:
            return found
        parent = d.parent
        if parent == d:
            break
        d = parent

    if not allow_lock_fallback:
        return None

    for lock_path in sorted(glob.glob("/dev/shm/claude-serial-*.lock")):
        cached = _read_lock(lock_path)
        if cached:
            found = _check_dir(Path(cached))
            if found:
                return found
    return None


def load_inventory(project_dir: str) -> Optional[dict]:
    """Load and shape-check `.dut-serial/inventory.json`; None when absent,
    unreadable, malformed, or not schema_version 1. Never raises."""
    path = Path(project_dir) / ".dut-serial" / INVENTORY_FILE
    try:
        inv = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(inv, dict) or inv.get("schema_version") != 1:
        return None
    return inv


def _pid_is_mcp_server(pid: int) -> bool:
    """Is the pid ACTUALLY a sermcp process?

    os.kill(pid, 0) only proves the pid exists — a recycled pid (the system
    reused the dead server's number for another process) would make a stale
    inventory look live and render old states. Identity instead (same two
    checks as lib._is_embedded_server and the JS pidIsMcpServer — parity
    matters, a divergence would make one renderer trust a snapshot another
    rejects):
      1. /proc/<pid>/comm contains "sermcp"; OR
      2. argv[0]'s basename (from /proc/<pid>/cmdline) starts with
         "sermcp" — catches argv[0]-masqueraded processes whose comm was
         truncated or rewritten.
    A missing /proc entry = dead → False. Stays stdlib-only and cheap: the
    cmdline is read ONLY when the comm check misses.
    """
    proc = Path("/proc") / str(pid)
    try:
        comm = (proc / "comm").read_text().strip()
        if "sermcp" in comm:
            return True
    except (FileNotFoundError, OSError, ValueError):
        return False
    try:
        argv0 = (proc / "cmdline").read_text(errors="replace").split("\0")[0]
        if argv0 and Path(argv0).name.startswith("sermcp"):
            return True
    except (FileNotFoundError, OSError, ValueError):
        return False
    return False


def inventory_is_live(inv: dict) -> bool:
    """Whether a live MCP server backs this inventory.

    TWO gates:
      1. Identity: the recorded mcp_pid must be alive AND be a
         sermcp process (recycled-pid protection).
      2. Freshness: when the server declared a heartbeat interval, the
         snapshot's written_at_unix must be within 3 × heartbeat_secs.
         The server rewrites the inventory on that cadence even when idle,
         so a stale written_at means the server died (kill -9 leaves no
         cleanup hook — without this gate the old state would render
         forever). Snapshots WITHOUT heartbeat_secs (dutabo list --json,
         pre-heartbeat servers) fall back to the identity gate alone —
         os.kill(pid, 0) stays the only trustworthy check for them.
    """
    pid = inv.get("mcp_pid")
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except (OSError, ProcessLookupError):
        return False
    if not _pid_is_mcp_server(pid):
        return False
    heartbeat = inv.get("heartbeat_secs")
    if isinstance(heartbeat, int) and heartbeat > 0:
        written_at = inv.get("written_at_unix")
        if not isinstance(written_at, int):
            return False
        import time

        if time.time() - written_at > 3 * heartbeat:
            return False
    return True


def render_state(dut: dict) -> str:
    """Render one DUT's statusline text — the Rust pre-formatted ANSI
    `state_text` verbatim. "unknown"/"stopped" have empty text (no
    TargetState), so they render nothing, never a made-up healthy state."""
    return dut.get("state_text") or ""
