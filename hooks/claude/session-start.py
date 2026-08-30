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
  - Liveness is pid-identity (/proc) AND an HTTP /health probe — a pid that
    is alive but not answering gets SIGTERMed so a fresh server can spawn.
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
    _is_embedded_server,
    http_health_ok,
    mark_session_alive,
    owner_session_marker_exists,
    read_hook_payload,
    resolve_plugin_binary,
    sessions_lock,
    spawn_http_mcp,
    terminate_pid,
)
from owner import claim_owner, owner_is_alive, owner_path


def find_projects():
    """Discover the current project via the shared walk-up (TARGET_CONF env
    -> CWD walk-up for .dut-serial/inventory.json -> .target.jsonc).

    The /dev/shm lock fallback is DELIBERATELY disabled: it is cross-project
    leakage — a session with no config of its own must never discover (and
    then write locks / start servers for) another project.
    """
    root = find_project_root(allow_lock_fallback=False)
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


def _http_port_of_pid(pid: int) -> int | None:
    """The --http port of a spawned server, parsed from /proc/<pid>/cmdline.

    None when the process carries no --http flag (the stdio server Claude
    Code spawns from .mcp.json) — such a process cannot be health-probed
    and must be treated conservatively (alive), never reaped.
    """
    try:
        args = (Path("/proc") / str(pid) / "cmdline").read_text(errors="replace").split("\0")
    except (FileNotFoundError, OSError, ValueError):
        return None
    for i, arg in enumerate(args):
        if arg == "--http" and i + 1 < len(args) and ":" in args[i + 1]:
            try:
                return int(args[i + 1].rsplit(":", 1)[-1].split("/")[0])
            except ValueError:
                return None
        if arg.startswith("--http=") and ":" in arg:
            try:
                return int(arg.split("=", 1)[1].rsplit(":", 1)[-1].split("/")[0])
            except ValueError:
                return None
    return None


def _mcp_pid_is_serving(pid: int, port: int | None = None) -> bool:
    """Identity + serving check for one candidate MCP pid.

    True when the pid is a verified sermcp process AND either
    carries no HTTP port to probe (stdio server → assume alive; killing it
    would rip the server out of Claude Code's hands) or /health answers
    within 1s. False when identity fails or the probe fails.
    """
    if not _is_embedded_server(pid):
        return False
    probe_port = port if port is not None else _http_port_of_pid(pid)
    if probe_port is None:
        return True
    return http_health_ok(probe_port, timeout=1.0)


def _reap_unhealthy_server(pid: int, pid_file, why: str) -> None:
    """SIGTERM (3s grace, SIGKILL fallback) a pid that was VERIFIED as one
    of our MCP processes but stopped serving, then drop its stale pidfile.
    Reports to stderr so the respawn is visible in the session log."""
    print(
        f"[sermcp] stale MCP server pid {pid} ({why}) — "
        f"terminating and respawning",
        file=sys.stderr,
    )
    terminate_pid(pid, grace_secs=3.0)
    if pid_file is not None:
        try:
            if pid_file.read_text().strip() == str(pid):
                pid_file.unlink()
        except OSError:
            pass


def _read_pid_file(pid_file) -> int | None:
    try:
        pid = int(pid_file.read_text().strip())
        return None if pid == os.getpid() else pid
    except (ValueError, OSError):
        return None


def _drop_stale_pid_file(pid_file, pid: int) -> None:
    try:
        if pid_file.read_text().strip() == str(pid):
            pid_file.unlink()
    except OSError:
        pass


def mcp_running(project_dir):
    """Check if a sermcp process is already SERVING this project.

    A pid-existence check is not enough (the process may be wedged or have
    lost its port): every candidate is identity-checked via /proc and, when
    it carries an --http port, health-probed (GET /health, 1s). A pid that
    is alive per /proc but not answering is reaped so a fresh server can
    spawn. Checks PID files first (fast path), then falls back to /proc
    scanning.
    """
    proj = Path(project_dir)
    # Fast path: check root and per-DUT PID files (a missing .dut-serial
    # means nothing is running — degrade to the /proc scan, never raise).
    try:
        pid_dirs = [proj / ".dut-serial"] + [
            d for d in (proj / ".dut-serial").iterdir()
            if d.is_dir() and (d / "mcp.pid").exists()
        ]
    except OSError:
        pid_dirs = [proj / ".dut-serial"]
    for pid_dir in pid_dirs:
        pid_file = pid_dir / "mcp.pid"
        if pid_file.exists():
            pid = _read_pid_file(pid_file)
            if pid is None:
                continue
            if _mcp_pid_is_serving(pid):
                return True
            if _is_embedded_server(pid):
                _reap_unhealthy_server(
                    pid, pid_file, f"port {_http_port_of_pid(pid)} not answering /health"
                )
            else:
                # Dead or recycled pid — the pidfile is stale either way.
                _drop_stale_pid_file(pid_file, pid)
    # Fallback: scan /proc for MCP processes with matching cwd
    try:
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                pid = int(entry.name)
                if pid == os.getpid():
                    continue
                if not _is_embedded_server(pid):
                    continue
                cwd = os.readlink(f"{entry}/cwd")
                if Path(cwd).resolve() == proj.resolve():
                    if _mcp_pid_is_serving(pid):
                        return True
                    port = _http_port_of_pid(pid)
                    if port is not None:
                        _reap_unhealthy_server(pid, None, f"port {port} not answering /health")
            except (OSError, FileNotFoundError, ValueError):
                continue
    except OSError:
        pass
    return False


def _kill_stale_mcp_on_port(port, project_dir: str):
    """Kill a sermcp process listening on the given port.

    Validates the PID is actually a sermcp process (via /proc/comm)
    AND that its cwd resolves to THIS project before killing — never kills a
    process serving another project that happens to hold the port.
    """
    proj_resolved = str(Path(project_dir).resolve())
    result = subprocess.run(
        ["ss", "-tlnpH", f"sport = :{port}"],
        capture_output=True, text=True
    )
    for line in result.stdout.splitlines():
        if "pid=" not in line:
            continue
        old_pid = line.split("pid=")[-1].split(",")[0]
        try:
            pid = int(old_pid)
        except ValueError:
            continue
        # Validate before killing.
        if not _is_embedded_server(pid):
            continue
        try:
            cwd = os.readlink(f"/proc/{pid}/cwd")
        except OSError:
            continue
        if str(Path(cwd).resolve()) != proj_resolved:
            continue
        try:
            os.kill(pid, 15)
        except OSError:
            pass
    time.sleep(0.5)


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
    """Ensure an HTTP MCP server is running for this project.

    Starts one if not already running (or if the recorded one stopped
    serving — mcp_running reaps it). Never kills healthy processes.
    The MCP self-terminates on idle; session-stop kills it when the last
    session leaves, so this hook re-spawns on the next session start.
    """
    # Already running (identity + /health verified) for this project?
    if mcp_running(project_dir):
        return

    # Port in use by something else (stale MCP from another project)?
    if is_port_in_use(port):
        _kill_stale_mcp_on_port(port, project_dir)
        if is_port_in_use(port):
            # Kill failed — port held by a non-MCP process we must not
            # touch. Say so: a silent return here leaves Claude Code's
            # HTTP transport failing with no explanation anywhere.
            print(
                f"[sermcp] HTTP port {port} is held by an unrelated "
                "process that will not be touched — refusing to spawn the "
                "MCP server on it",
                file=sys.stderr,
            )
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
    session_id = read_hook_payload().get("session_id")

    projects = find_projects()
    if not projects:
        # No .target.jsonc anywhere up the tree — the MCP must not start.
        # Also disarm a stale committed .mcp.json entry so Claude Code
        # itself does not spawn the server on the next session either.
        _strip_sermcp_from_mcp_json()
        sys.exit(0)

    for proj in projects:
        dut_dir = os.path.join(proj, ".dut-serial")
        os.makedirs(dut_dir, exist_ok=True)

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
        mcp_json_path = os.path.join(proj, ".mcp.json")
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
                "TARGET_DUT_NAME" not in dumped and "TARGET_DUT_ALIAS" not in dumped,
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

    # ── P4: the session lock claim — O_EXCL, 4-line format, self-reaping.
    #    D8/D9 rules: canonicalized hash; live owner → observe; dead owner →
    #    reap + claim; old-format lock with an existing dir → NEVER touched.
    import inventory as _inv
    import hashlib

    _MISSING = object()

    claim = globals().get("claim_session_lock")
    if claim is None:
        check(
            "P4 session lock claim exists (claim_session_lock)",
            False,
            "not implemented",
        )
    else:
        with tempfile.TemporaryDirectory() as td:
            base = Path(td)
            proj = base / "p4-proj"
            proj.mkdir()
            (proj / ".target.jsonc").write_text("{}")
            scratch = base / "session.lock"
            real_lp = getattr(_inv, "project_lock_path", None)
            _inv.project_lock_path = lambda p: str(scratch)
            try:
                # 1. O_EXCL claim writes the 4-line format.
                claim(str(proj), "sess-1", os.getpid())
                lines = scratch.read_text().splitlines()
                check(
                    "P4 claim writes the 4-line lock format",
                    len(lines) == 4,
                    scratch.read_text(),
                )
                if len(lines) == 4:
                    check(
                        "P4 line 0 is the canonical project dir",
                        lines[0] == str(Path(proj).resolve()),
                        lines[0],
                    )
                    check(
                        "P4 line 1 is the owner pid",
                        lines[1] == str(os.getpid()),
                        lines[1],
                    )
                    check(
                        "P4 line 2 is the session id",
                        lines[2] == "sess-1",
                        lines[2],
                    )
                    try:
                        float(lines[3])
                        ts_ok = True
                    except ValueError:
                        ts_ok = False
                    check("P4 line 3 is a unix timestamp", ts_ok, lines[3])

                # 2. Live owner → observe, file untouched.
                before = scratch.read_text()
                claim(str(proj), "sess-2", os.getpid())
                check(
                    "P4 live owner → observe (file untouched)",
                    scratch.read_text() == before,
                )

                # 3. Dead owner → reap + claim.
                scratch.write_text("\n".join([
                    str(Path(proj).resolve()),
                    "99999999",
                    "other-sess",
                    str(int(time.time())),
                ]))
                claim(str(proj), "sess-3", os.getpid())
                lines = scratch.read_text().splitlines()
                check(
                    "P4 dead owner is reaped and re-claimed",
                    len(lines) == 4
                    and lines[1] == str(os.getpid())
                    and lines[2] == "sess-3",
                    scratch.read_text(),
                )

                # 4. Old-format (path-only) lock with existing dir → NEVER
                #    touched (D9 — the live-session protection).
                scratch.write_text(str(proj))
                claim(str(proj), "sess-4", os.getpid())
                check(
                    "P4 old-format lock is never touched",
                    scratch.read_text() == str(proj),
                )

                # 5. The claim never writes .session-pid (B3 deleted).
                check(
                    "P4 never writes .dut-serial/.session-pid",
                    not (proj / ".dut-serial" / ".session-pid").exists(),
                )
            finally:
                if real_lp is not None:
                    _inv.project_lock_path = real_lp
                else:
                    del _inv.project_lock_path

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

    # ── P7 (B12): port-kill validation — a sermcp pid whose cwd is
    #    NOT this project must never be killed (cross-project R1 vector on
    #    collided ports); a stale same-project server IS killed; a
    #    non-sermcp pid is never touched.
    FAKE_PID = 1234567

    class _FakeOs:
        def __init__(self):
            self._real = os
            self.kill_calls = []
            self.fake_cwd = None

        def __getattr__(self, name):
            return getattr(self._real, name)

        def readlink(self, path):
            if path == f"/proc/{FAKE_PID}/cwd":
                return self.fake_cwd
            return self._real.readlink(path)

        def kill(self, pid, sig):
            self.kill_calls.append((pid, sig))

    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        proj = base / "p7-proj"
        other = base / "p7-other"
        proj.mkdir()
        other.mkdir()

        fake = _FakeOs()
        real_os = globals().get("os")
        real_run = getattr(globals().get("subprocess"), "run", None)
        real_sleep = getattr(globals().get("time"), "sleep", None)
        real_embedded = globals().get("_is_embedded_server")
        try:
            globals()["os"] = fake
            import types

            def _fake_ss_run(*args, **kwargs):
                return types.SimpleNamespace(
                    stdout=(
                        'LISTEN 0 4096 127.0.0.1:3999 0.0.0.0:* '
                        f'users:((\\"sermcp\\",pid={FAKE_PID},fd=3))'
                    )
                )

            subprocess.run = _fake_ss_run
            time.sleep = lambda _s: None

            globals()["_is_embedded_server"] = lambda pid: True
            fake.fake_cwd = str(other)
            _kill_stale_mcp_on_port(3999, str(proj))
            check(
                "P7 never kills a sermcp pid whose cwd is another project",
                fake.kill_calls == [],
                f"kill calls: {fake.kill_calls!r}",
            )

            fake.fake_cwd = str(proj)
            _kill_stale_mcp_on_port(3999, str(proj))
            check(
                "P7 kills a stale same-project server on the port",
                fake.kill_calls == [(FAKE_PID, 15)],
                f"kill calls: {fake.kill_calls!r}",
            )

            fake.kill_calls.clear()
            globals()["_is_embedded_server"] = lambda pid: False
            fake.fake_cwd = str(proj)
            _kill_stale_mcp_on_port(3999, str(proj))
            check(
                "P7 never kills a non-sermcp pid",
                fake.kill_calls == [],
                f"kill calls: {fake.kill_calls!r}",
            )
        finally:
            if real_os is not None:
                globals()["os"] = real_os
            if real_run is not None:
                subprocess.run = real_run
            if real_sleep is not None:
                time.sleep = real_sleep
            if real_embedded is not None:
                globals()["_is_embedded_server"] = real_embedded
            else:
                globals().pop("_is_embedded_server", None)

    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(_selftest())
    main()
