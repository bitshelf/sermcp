#!/usr/bin/env python3
"""SessionStart hook — discover the current project and start the MCP HTTP server.

Projects only need `.mcp.json` + `.target.jsonc`. No per-project scripts.

In **stdio** mode (the default, `.mcp.json` has `command` not `url`), Claude Code
itself spawns the MCP server from `.mcp.json` — this hook does NOT start it.
In **HTTP** mode (`.mcp.json` has `url`), this hook starts the HTTP server.

Data bootstrap: once per session this hook runs `dutabo list --json` (the ONE
latency-tolerant subprocess) to enumerate names/ports/dev-hosts BEFORE any
server exists, and caches the result into `.dut-serial/inventory.json`
(written_by="dutabo"). Per-DUT ports come from that Rust-computed payload.

Lifecycle contract: the HTTP servers self-terminate after their
idle timeout (~10min without MCP/WebSocket activity), and session-stop
terminates them PROMPTLY when the last session leaves. Two mechanisms here:
  - A held project lock prevents the hook from starting another server.
  - Every session (owner AND guest) leaves a marker in
    `.dut-serial/sessions/<session_id>`; session-stop counts them to decide
    when "the last session is out".
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

# Shared project discovery (inventory.json -> .target.jsonc) so every hook
# agrees on what a project is.
from typing import Optional

from inventory import find_project_root, load_inventory, inventory_is_live
from lib import (
    agent_session_process,
    detect_endpoint_contention,
    mark_session_alive,
    owner_session_marker_exists,
    project_locked,
    read_hook_payload,
    resolve_plugin_binary,
    sessions_lock,
    spawn_http_mcp,
)
from owner import claim_owner, owner_is_alive, owner_path


def find_projects(payload: Optional[dict] = None):
    """Discover the current project via the shared walk-up (TARGET_CONF env
    -> CWD walk-up for .dut-serial/inventory.json -> .target.jsonc).

    The /dev/shm lock fallback is DELIBERATELY disabled: it is cross-project
    leakage — a session with no config of its own must never discover (and
    then write locks / start servers for) another project.
    """
    cwd = payload.get("cwd") if isinstance(payload, dict) else None
    root = find_project_root(allow_lock_fallback=False, start=Path(cwd) if isinstance(cwd, str) and cwd else None)
    if root is None:
        return []
    return [root]


def read_mcp_port(project_dir):
    """Read HTTP port from the project's `.mcp.json`. Returns int or None.

    Only HTTP-mode configs have a `url` field. stdio-mode configs return None,
    which means the MCP is spawned by Claude Code itself and this hook does not
    start a server.
    """
    mcp_json = Path(project_dir) / ".mcp.json"
    if not mcp_json.exists():
        return None
    try:
        cfg = json.loads(mcp_json.read_text())
        url = cfg.get("mcpServers", {}).get("sermcp", {}).get("url", "")
        # "http://localhost:3000/mcp" → 3000
        if ":" in url:
            return int(url.rsplit(":", 1)[-1].split("/")[0])
    except (json.JSONDecodeError, ValueError, KeyError):
        pass
    return None


def is_port_in_use(port):
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.connect(("127.0.0.1", port))
        s.close()
        return True
    except (ConnectionRefusedError, OSError):
        return False


def _plugin_binary() -> str | None:
    """Locate the installed sermcp plugin binary (absolute path).

    Preference order: the Codex-style plugin install dir (user's choice of
    "plugin path"), then the current plugin root (CLAUDE_PLUGIN_ROOT, injected
    by Claude Code for plugin hooks), then any cached embedded-debug plugin
    version. Writing the resolved absolute path into .mcp.json avoids ${VAR}
    expansion failures on reconnect (e.g. CLAUDE_PLUGIN_ROOT → ENOENT).
    Delegates to lib.resolve_plugin_binary — the single lookup shared with
    the user-prompt-submit restart path.
    """
    return _binary_named("sermcp")


def _binary_named(name: str) -> str | None:
    """Locate a plugin binary by name: same candidate dirs as _plugin_binary
    plus PATH (dutabo is usually on PATH or beside the MCP binary)."""
    return resolve_plugin_binary(name)


def _dutabo_binary() -> str | None:
    """Locate the dutabo CLI binary (PATH, plugin dirs, or beside the MCP)."""
    binary = _binary_named("dutabo")
    if binary:
        return binary
    mcp = _plugin_binary()
    if mcp:
        sibling = Path(mcp).with_name("dutabo")
        if sibling.is_file():
            return str(sibling)
    return None


def _run_dutabo_list(project_dir: str):
    """Run `dutabo list --json` once (the ONE latency-tolerant subprocess)
    and return the parsed inventory payload, or None on any failure — the
    caller prints a visible warning and skips per-DUT spawns."""
    binary = _dutabo_binary()
    if not binary:
        return None
    try:
        result = subprocess.run(
            [binary, "list", "--json"],
            cwd=project_dir,
            capture_output=True, text=True, timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        return None
    return payload


def _cache_bootstrap(project_dir: str, bootstrap: dict) -> None:
    """Cache the CLI bootstrap into .dut-serial/inventory.json
    (written_by="dutabo" is already in the payload). NEVER clobbers a live
    server's inventory — its mcp_pid is the liveness signal the statusline
    gates on, and the server refreshes the file itself."""
    existing = load_inventory(project_dir)
    if existing is not None and inventory_is_live(existing):
        return
    inv_path = Path(project_dir) / ".dut-serial" / "inventory.json"
    try:
        inv_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = inv_path.with_name(".inventory.json.tmp")
        tmp.write_text(json.dumps(bootstrap, indent=2))
        os.replace(tmp, inv_path)
    except OSError:
        pass


def _ensure_http_mcp(project_dir: str, port: int) -> None:
    """Start a session-bound HTTP server when the project has no owner."""
    if agent_session_process() is None:
        print("[sermcp] Cannot identify the Agent session process; use host-managed stdio.", file=sys.stderr)
        return
    if project_locked(project_dir):
        return
    if is_port_in_use(port):
        print(f"[sermcp] HTTP port {port} is occupied. Check the project's MCP port.", file=sys.stderr)
        return

    binary = _plugin_binary()
    if not binary:
        return

    target_conf = os.path.join(project_dir, ".target.jsonc")
    target_conf_env = os.environ.get("TARGET_CONF", target_conf)

    spawn_http_mcp(
        project_dir,
        port,
        target_conf=target_conf_env,
        binary=binary,
    )


def _strip_sermcp_from_mcp_json() -> None:
    """Remove the sermcp entry from the nearest .mcp.json when no
    project (.target.jsonc) is found on the walk-up, so Claude Code does not
    keep spawning the MCP from a stale committed config (R5 gating fix).
    Inert while a .target.jsonc exists anywhere on the walk-up.

    Safety valves: the walk stops at the git worktree boundary, and when
    that worktree still CONTAINS a tracked .target.jsonc anywhere (a
    sibling project sharing this .mcp.json), the entry is left alone with
    a warning — stripping it would break MCP for the sibling. The rewrite
    itself is atomic (tmp + rename): Claude Code may read the file
    concurrently and must never see a truncated config.
    """
    import subprocess

    def _git(args: list) -> Optional[str]:
        try:
            proc = subprocess.run(
                ["git", *args], capture_output=True, text=True, timeout=5
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return proc.stdout.strip() if proc.returncode == 0 else None

    d = Path.cwd().resolve()
    top = _git(["rev-parse", "--show-toplevel"])
    worktree = Path(top).resolve() if top else None
    while True:
        mcp_json = d / ".mcp.json"
        if mcp_json.is_file():
            if worktree is not None:
                tracked = _git(["-C", str(worktree), "ls-files", "*.target.jsonc"])
                if tracked:
                    print(
                        "[sermcp] .mcp.json keeps its sermcp entry: "
                        "this worktree still contains .target.jsonc project(s) "
                        "elsewhere — removing it would break their MCP",
                        file=sys.stderr,
                    )
                    return
            try:
                cfg = json.loads(mcp_json.read_text())
            except (json.JSONDecodeError, IOError):
                return
            servers = cfg.get("mcpServers", {})
            dc = servers.get("sermcp", {})
            cmd = dc.get("command", "")
            if dc and (not cmd or "sermcp" in cmd):
                servers.pop("sermcp", None)
                tmp = mcp_json.with_name(f".{mcp_json.name}.tmp-{os.getpid()}")
                try:
                    tmp.write_text(json.dumps(cfg, indent=2) + "\n", encoding="utf-8")
                    os.replace(tmp, mcp_json)
                except OSError:
                    try:
                        tmp.unlink()
                    except OSError:
                        pass
            return
        if d.parent == d:
            return
        if worktree is not None and d == worktree:
            # Never walk above the worktree boundary: configs up there do
            # not belong to this tree of projects.
            return
        d = d.parent


def _owner_marker_gone(project_dir: str, rec: dict) -> bool:
    """Guest-takeover gate: the owner record claims ALIVE (owner_is_alive)
    but the owning session's marker file no longer exists.

    A live record whose owner marker is missing means the owner session
    vanished without running session-stop (client crash) while its MCP
    process still runs — the next arrival may take the record over.
    Anything less certain (vacant record — claim_owner already re-claims
    it; record without a session id; marker still present) → False.
    """
    if not isinstance(rec, dict):
        return False
    owner_sid = rec.get("owner_session_id")
    if not isinstance(owner_sid, str) or not owner_sid:
        return False
    if not owner_is_alive(rec):
        return False
    return not owner_session_marker_exists(project_dir, owner_sid)


def main():
    import hashlib
    from pathlib import Path

    # Session identity from the hook input (Claude Code passes JSON on stdin);
    # the owner record stores it so later sessions can render owner markers.
    payload = read_hook_payload()
    session_id = payload.get("session_id")

    projects = find_projects(payload)
    if not projects:
        # No .target.jsonc anywhere up the tree — the MCP must not start.
        # Also disarm a stale committed .mcp.json entry so Claude Code
        # itself does not spawn the server on the next session either.
        _strip_sermcp_from_mcp_json()
        sys.exit(0)

    for proj in projects:
        if (Path(proj) / ".dut-serial" / "stopped").exists():
            print(f"[sermcp] {proj}: stopped; human dutabo start or dutabo resume enables startup.", file=sys.stderr)
            continue
        dut_dir = os.path.join(proj, ".dut-serial")
        os.makedirs(dut_dir, exist_ok=True)

        # Surface local endpoint conflicts without terminating foreign owners.
        for alert in detect_endpoint_contention(proj):
            print(f"[sermcp] {alert}", file=sys.stderr)

        # ── Ownership claim (multi-agent isolation) ──
        # The FIRST agent exclusively owns the MCP lifecycle (start/restart);
        # later agents are guests that only connect to the existing MCP. The
        # record anchors on owner_mcp_pid: when the owner's MCP dies the
        # ownership goes vacant and the next agent to arrive re-claims
        # (liveness following).
        # The lock serializes claim + marker write + the owner/guest spawn
        # decision against a concurrent session-stop's "am I last?" check —
        # without it the stop can empty-check, see our marker land, and kill
        # the server we just classified ourselves against (project left with
        # zero servers and no one to respawn them).
        with sessions_lock(proj):
            rec = claim_owner(proj, os.getpid(), session_id)
            is_owner = bool(session_id) and rec.get("owner_session_id") == session_id

            # ── Session reference count: every session (owner AND guest) that
            #    reaches the point of using this project's server leaves a
            #    marker in .dut-serial/sessions/<session_id>. session-stop
            #    removes it on exit and terminates the servers when the LAST
            #    marker is gone (the Rust idle-timeout is the backstop for
            #    clients that crash without running SessionStop). ──
            if session_id:
                mark_session_alive(proj, session_id, "owner" if is_owner else "guest")

            # ── Guest takeover (conservative): the record claims alive but the
            #    owning session's marker no longer exists → the owner session
            #    is gone; re-claim instead of deadlocking as a guest forever.
            #    Only the missing-marker signal triggers this — never a live
            #    owner, never an unidentified record. ──
            if not is_owner and _owner_marker_gone(proj, rec):
                print(
                    f"[sermcp] owner session {rec.get('owner_session_id')} of "
                    f"{proj} is gone (marker missing) — taking over ownership.",
                    file=sys.stderr,
                )
                try:
                    owner_path(proj).unlink()
                except OSError:
                    pass
                rec = claim_owner(proj, os.getpid(), session_id)
                is_owner = bool(session_id) and rec.get("owner_session_id") == session_id
                if is_owner and session_id:
                    mark_session_alive(proj, session_id, "owner")

            if not is_owner:
                print(
                    f"[sermcp] guest session in {proj}: MCP lifecycle owned "
                    f"by session {rec.get('owner_session_id')} (mcp_pid "
                    f"{rec.get('owner_mcp_pid')}) — connect only, no spawn.",
                    file=sys.stderr,
                )
                # A guest never writes locks / starts / restarts anything here.
                continue

        # Owner only from here: the /dev/shm hint lock is a project-level
        # advertisement written by the owner alone (guests must not touch it).
        h = hashlib.md5(str(Path(proj).resolve()).encode()).hexdigest()[:8]
        lock_file = f"/dev/shm/claude-serial-{h}.lock"
        with open(lock_file, "w") as f:
            f.write(proj)

        # ── Auto-generate .mcp.json (stdio) when the project has a
        #    .target.jsonc but no sermcp entry yet — the template
        #    pins TARGET_CONF to the absolute .target.jsonc path ──
        # Path, not os.path.join: the atomic-write branch below calls
        # .with_name() on it (a str crashes the hook with AttributeError
        # in any project whose .mcp.json has no sermcp entry yet).
        mcp_json_path = Path(proj) / ".mcp.json"
        existing_cfg = {}
        if os.path.exists(mcp_json_path):
            try:
                with open(mcp_json_path) as f:
                    existing_cfg = json.load(f)
            except (json.JSONDecodeError, IOError):
                pass
        mcp_servers = existing_cfg.setdefault("mcpServers", {})
        dc = mcp_servers.get("sermcp", {})

        if not dc:
            binary = _plugin_binary()
            if binary:
                mcp_servers["sermcp"] = {
                    "command": binary,
                    "args": [],
                    "env": {
                        "RUST_LOG": "info",
                        "TARGET_CONF": os.path.join(proj, ".target.jsonc"),
                    },
                }
                dc = mcp_servers["sermcp"]
                # Atomic: Claude Code may read this file concurrently — a
                # truncated write would break EVERY configured server.
                _mcp_json_tmp = mcp_json_path.with_name(
                    f".{mcp_json_path.name}.tmp-{os.getpid()}"
                )
                try:
                    _mcp_json_tmp.write_text(
                        json.dumps(existing_cfg, indent=2) + "\n", encoding="utf-8"
                    )
                    os.replace(_mcp_json_tmp, mcp_json_path)
                except OSError:
                    try:
                        _mcp_json_tmp.unlink()
                    except OSError:
                        pass

        # Bootstrap `dutabo list --json` once per session to seed inventory.
        # It never drives one-process-per-DUT spawning: the project path is
        # the MCP ownership boundary.
        bootstrap = _run_dutabo_list(proj)
        if bootstrap is None:
            print(
                f"[sermcp] `dutabo list --json` failed for {proj} — "
                f"skipping HTTP MCP startup.",
                file=sys.stderr,
            )
            continue
        _cache_bootstrap(proj, bootstrap)
        project_port = bootstrap.get("mcp_http_port") or read_mcp_port(proj)

        # HTTP configurations need one detached process. In stdio mode the
        # client owns that one process, which exposes its control HTTP port
        # itself; this hook must not create a twin.
        if dc.get("url") and project_port:
            # Serialized against concurrent session-starts: without the
            # lock two hooks can both observe "not running" (the winner's
            # spawn has not bound its port yet) and double-spawn — and
            # the loser's reaping path could then kill the winner inside
            # its startup grace window.
            with sessions_lock(proj):
                _ensure_http_mcp(proj, project_port)

    sys.exit(0)


def _selftest() -> int:
    """Project-detection regression checks (no TTY needed).

    Run with: `uv run hooks/claude/session-start.py --selftest`
    - find_projects: walk-up finds a .target.jsonc project; TARGET_CONF
      overrides to the config's parent dir; no config anywhere yields no
      projects.
    """
    import tempfile

    failed = 0

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal failed
        if cond:
            print(f"OK    {name}")
        else:
            print(f"FAIL  {name}{(' — ' + detail) if detail else ''}")
            failed += 1

    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        proj = base / "proj"
        other = base / "other"
        proj.mkdir()
        other.mkdir()
        (proj / ".target.jsonc").write_text("{}")

        old_conf = os.environ.pop("TARGET_CONF", None)
        real_glob = None
        try:
            import inventory as _inv
            real_glob = _inv.glob.glob
            _inv.glob.glob = lambda pattern: []  # silence lock fallback

            os.chdir(proj)
            check(
                "walk-up finds a .target.jsonc project",
                find_projects() == [str(proj)],
                f"got {find_projects()!r}",
            )

            os.environ["TARGET_CONF"] = str(proj / ".target.jsonc")
            check(
                "TARGET_CONF override is honored",
                find_projects() == [str(proj)],
                f"got {find_projects()!r}",
            )

            (proj / ".target.jsonc").unlink()
            os.environ.pop("TARGET_CONF", None)
            os.chdir(other)
            check(
                "no config anywhere yields no projects",
                find_projects() == [],
                f"got {find_projects()!r}",
            )

            # The project-scoped spawn pins TARGET_CONF and carries no DUT
            # selector that could create a separate ownership domain.
            (proj / ".target.jsonc").write_text("{}")
            env_dump = base / "env.txt"
            fake_bin = base / "fake-mcp"
            fake_bin.write_text(f"#!/bin/sh\nenv > {env_dump}\n")
            fake_bin.chmod(0o755)
            from unittest.mock import patch
            from lib import _session_process_identity
            with patch.dict(os.environ, {"SERMCP_SESSION_PID": str(os.getpid()),
                                         "SERMCP_SESSION_START_TICKS": _session_process_identity(os.getpid())[0]}):
                spawn_http_mcp(str(proj), 2001, binary=str(fake_bin))
            time.sleep(0.3)
            dumped = env_dump.read_text()
            check(
                "spawn env pins TARGET_CONF",
                f"TARGET_CONF={proj / '.target.jsonc'}" in dumped,
                dumped,
            )
            check(
                "spawn env carries no DUT selector",
                "TARGET_DUT_NAME" not in dumped,
                dumped,
            )
        finally:
            if real_glob is not None:
                _inv.glob.glob = real_glob
            os.chdir(base)
            if old_conf is not None:
                os.environ["TARGET_CONF"] = old_conf
            else:
                os.environ.pop("TARGET_CONF", None)

    # Session startup must refuse replacement before touching existing processes
    # when no persistent session identity can be established.
    from unittest.mock import patch
    with patch.dict(globals(), {"agent_session_process": lambda: None}), patch.dict(globals(), {"project_locked": lambda _: (_ for _ in ()).throw(AssertionError("must not inspect an existing server"))}):
        _ensure_http_mcp("/unused/project", 3000)
        check("unknown session identity refuses HTTP startup before process inspection", True)

    _MISSING = object()
    # ── P4 discovery: find_projects must resolve from the payload cwd,
    #    never the process CWD (ROOT CAUSE A).
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        proj = base / "p4-proj"
        other = base / "p4-other"
        proj.mkdir()
        other.mkdir()
        (proj / ".target.jsonc").write_text("{}")
        old_conf = os.environ.pop("TARGET_CONF", None)
        try:
            os.chdir(other)
            detail = ""
            try:
                got = find_projects(payload={"cwd": str(proj), "session_id": "p4-sess"})
            except TypeError as e:
                got = _MISSING
                detail = f"TypeError: {e}"
            check(
                "P4 find_projects resolves from payload cwd (not process CWD)",
                got == [str(proj)],
                f"got {got!r}{(' — ' + detail) if detail else ''}",
            )
        finally:
            os.chdir(base)
            if old_conf is not None:
                os.environ["TARGET_CONF"] = old_conf
            else:
                os.environ.pop("TARGET_CONF", None)

    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(_selftest())
    main()
