#!/usr/bin/env python3
"""Shared utilities for embedded-debug hooks.

Two tiers, kept deliberately separate:

Hot path (statusline / pre-tool-use — <2ms, zero network I/O):
project discovery delegates to inventory.find_project_root; MCP liveness
is the inventory mcp_pid via /proc identity (the pid the server itself
recorded is the authority). Python parses ZERO config: the single data
source is the Rust-written `.dut-serial/inventory.json` (see inventory.py).

Lifecycle path (session-start / session-stop / user-prompt-submit restart
— latency-tolerant): the helpers below this line MAY touch the network
(one short /health probe) or spawn/kill processes. The server
self-terminates after its idle timeout (~10min of no MCP/WebSocket
activity — and note every request, /health included, bumps that idle
timer, so these helpers must never probe on a cadence); they own the
deterministic parts of the lifecycle: session reference counting
(`.dut-serial/sessions/<session_id>` markers — the last session out
terminates the project's servers, serialized through `sessions_lock`)
and the canonical detached spawn.
"""

from __future__ import annotations

import contextlib
import os
import sys
from pathlib import Path
from typing import Callable, Optional

_HOOK_DIR = Path(__file__).resolve().parent
if str(_HOOK_DIR) not in sys.path:
    sys.path.insert(0, str(_HOOK_DIR))

import inventory as _inventory


def find_project_dir(
    allow_lock_fallback: bool = True,
    start: Optional[Path] = None,
) -> Optional[str]:
    """Find the project root (shared discovery in inventory.py).

    Session-stop cleanup is a harmless no-op when there is no project.
    Display paths (statusline) pass `allow_lock_fallback=False`: the
    /dev/shm lock fallback is cross-project leakage — a session with no
    config of its own must never render another project's DUT state.
    `start` overrides the walk-up origin (the hook payload's cwd).
    """
    return _inventory.find_project_root(
        allow_lock_fallback=allow_lock_fallback, start=start
    )


def _is_embedded_server(pid: int) -> bool:
    """Check if a PID belongs to a sermcp server process.

    Matches the executable basename, not an arbitrary cmdline substring, so a
    shell that merely mentions the server name cannot make stale state appear
    live.

    Used only by the latency-TOLERANT hooks (session-start, the
    user-prompt-submit restart path) that must verify a process before
    killing it — the hot paths never touch /proc.
    """
    try:
        comm = (Path("/proc") / str(pid) / "comm").read_text().strip()
        if "sermcp" in comm:
            return True
        argv0 = (Path("/proc") / str(pid) / "cmdline").read_text().split("\0")[0]
        if Path(argv0).name.startswith("sermcp"):
            return True
    except (FileNotFoundError, OSError):
        pass
    return False


def _repair_stale_pid_file(project_dir: str, pid: int) -> None:
    """Repair the project PID file after verifying the inventory PID is alive."""
    pid_dir = Path(project_dir) / ".dut-serial"
    if pid_dir.is_dir():
        try:
            (pid_dir / "mcp.pid").write_text(str(pid))
        except OSError:
            pass


def check_mcp_alive(project_dir: str) -> bool:
    """Check if the MCP server is alive: the inventory's mcp_pid via
    os.kill(pid, 0). The pid is recorded by the server itself when it
    writes inventory.json, so a stale pidfile elsewhere can never produce
    a false positive. Repairs stale mcp.pid files on a live hit."""
    inv = _inventory.load_inventory(project_dir)
    if inv is not None and _inventory.inventory_is_live(inv):
        _repair_stale_pid_file(project_dir, int(inv["mcp_pid"]))
        return True
    return False


# ── Lifecycle helpers (latency-TOLERANT paths only — see module docstring) ──

def http_health_ok(port: int, timeout: float = 1.0) -> bool:
    """GET /health on 127.0.0.1:<port> — is the HTTP MCP server answering?

    A pid-existence check cannot distinguish a live server from a wedged
    one (deadlock, half-dead tokio task, port lost to another process),
    so the lifecycle hooks verify identity via /proc AND serving via this
    probe before trusting a pidfile. stdlib-only (hooks must never grow
    third-party imports); any connection/HTTP error → False, never raises.
    NOTE: this is a DIAGNOSTIC probe — never call it on a cadence, or it
    becomes a keep-alive that defeats the server's idle timeout.
    """
    import http.client

    if not isinstance(port, int) or not (0 < port <= 65535):
        return False
    try:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
        try:
            conn.request("GET", "/health")
            resp = conn.getresponse()
            ok = 200 <= resp.status < 300
            resp.read()  # drain so the socket closes cleanly
            return ok
        finally:
            conn.close()
    except (OSError, ValueError, http.client.HTTPException):
        # HTTPException (BadStatusLine, IncompleteRead, ...) is NOT an
        # OSError subclass: a non-HTTP process holding the port raises it,
        # and letting it propagate crashes every caller on each invocation.
        return False


def resolve_plugin_binary(name: str) -> Optional[str]:
    """Locate a plugin binary by absolute path: PATH first, then the
    Codex-style plugin install cache, then CLAUDE_PLUGIN_ROOT (injected by
    Claude Code for plugin hooks), then the legacy plugin root. Returning
    the absolute path avoids ${VAR} expansion failures on reconnect."""
    import shutil

    candidates: list = []
    which = shutil.which(name)
    if which:
        candidates.append(which)
    cache_dir = Path.home() / ".claude" / "plugins" / "cache" / "local-res" / "embedded-debug"
    if cache_dir.is_dir():
        try:
            for version in sorted(cache_dir.iterdir(), reverse=True):
                candidates.append(str(version / "bin" / name))
        except OSError:
            pass
    plugin_root = os.environ.get("CLAUDE_PLUGIN_ROOT")
    if plugin_root:
        candidates.append(str(Path(plugin_root) / "bin" / name))
    candidates.append(str(Path.home() / "plugins" / "sermcp" / "bin" / name))
    for cand in candidates:
        if Path(cand).is_file():
            return cand
    return None


def spawn_http_mcp(
    project_dir: str,
    port: int,
    target_conf: Optional[str] = None,
    binary: Optional[str] = None,
) -> Optional[int]:
    """Spawn the one project-scoped HTTP MCP server detached.

    THE canonical spawn shared by session-start and the user-prompt-submit
    restart path, so both build identical argv/env: `--http 127.0.0.1:<port>`
    with TARGET_CONF pinned to THIS project's .target.jsonc. DUT selection
    never spawns another MCP process. Returns the child pid, or None when no
    binary was found or the spawn failed.
    """
    from subprocess import DEVNULL, Popen

    if binary is None:
        binary = resolve_plugin_binary("sermcp")
    if not binary:
        return None
    if target_conf is None:
        target_conf = os.path.join(project_dir, ".target.jsonc")
    env = {**os.environ, "TARGET_CONF": target_conf}
    env.pop("TARGET_DUT_NAME", None)
    env.pop("TARGET_DUT_ALIAS", None)
    try:
        child = Popen(
            [binary, "--http", f"127.0.0.1:{port}"],
            cwd=project_dir,
            env=env,
            stdout=DEVNULL,
            stderr=DEVNULL,
            start_new_session=True,
        )
    except OSError:
        return None
    return child.pid


def read_hook_payload() -> dict:
    """Read the hook's stdin JSON. Claude Code sends a top-level OBJECT;
    any other payload (list/number, or a parse failure) degrades to {}
    instead of crashing the hook with AttributeError on .get()."""
    import json

    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except (json.JSONDecodeError, OSError, ValueError):
        return {}
    return payload if isinstance(payload, dict) else {}


def pid_anchored_to_project(pid: int, project_dir: str) -> bool:
    """Project anchoring for a server pid: cwd == project root, or the
    process's TARGET_CONF env points inside the project (spawned by our
    hooks with an absolute path). Shared by session-stop AND the
    user-prompt-submit restart path — without it a port-collision can
    make one project kill another project's live server."""
    proj = Path(project_dir).resolve()
    try:
        cwd = Path(os.readlink(f"/proc/{pid}/cwd")).resolve()
        if cwd == proj:
            return True
    except OSError:
        return False  # no /proc entry or unreadable → cannot anchor
    try:
        environ = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
    except OSError:
        return False
    proj_str = str(proj)
    for entry in environ:
        if entry.startswith(b"TARGET_CONF="):
            value = entry[len(b"TARGET_CONF=") :].decode(errors="replace")
            if value == proj_str or value.startswith(proj_str + os.sep):
                return True
            try:
                if str(Path(value).resolve()) == proj_str or str(
                    Path(value).resolve()
                ).startswith(proj_str + os.sep):
                    return True
            except OSError:
                pass
    return False


def _sanitize_session_id(session_id: str) -> str:
    """Make a session id safe as a single path component."""
    return "".join(c if c.isalnum() or c in "-_." else "_" for c in session_id)[:128]


def session_marker_path(project_dir: str, session_id) -> Optional[Path]:
    """Marker file for one live session: `.dut-serial/sessions/<session_id>`."""
    if not session_id or not isinstance(session_id, str):
        return None
    return Path(project_dir) / ".dut-serial" / "sessions" / _sanitize_session_id(session_id)


def mark_session_alive(project_dir: str, session_id, role: str) -> None:
    """Create/refresh this session's marker (atomic tmp+rename).

    Content: ISO timestamp + role ("owner"/"guest"). Every session that
    reaches the point of USING a project's server must leave one — the
    session-stop hook counts them: the LAST marker removed triggers the
    project's server shutdown. Never raises."""
    marker = session_marker_path(project_dir, session_id)
    if marker is None:
        return
    try:
        marker.parent.mkdir(parents=True, exist_ok=True)
        import datetime

        stamp = datetime.datetime.now(datetime.timezone.utc).isoformat()
        tmp = marker.with_name(f".{marker.name}.tmp-{os.getpid()}")
        tmp.write_text(f"{stamp}\n{role}\n", encoding="utf-8")
        os.replace(tmp, marker)
    except OSError:
        pass


def remove_session_marker(project_dir: str, session_id) -> bool:
    """Remove this session's marker; True when a marker was removed."""
    marker = session_marker_path(project_dir, session_id)
    if marker is None:
        return False
    try:
        marker.unlink()
        return True
    except FileNotFoundError:
        return False
    except OSError:
        return False


@contextlib.contextmanager
def sessions_lock(project_dir: str, timeout: float = 10.0):
    """Serialize session-lifecycle decisions across concurrent sessions.

    Without mutual exclusion, session-stop's "am I the last session?"
    check races a concurrent session-start's marker write: the stop can
    empty-check BEFORE the start's marker lands and then kill the server
    the fresh session just classified itself against (leaving the project
    with zero servers and no one to respawn them). Holders of this lock:
    session-start (marker write + owner/guest spawn decision) and
    session-stop (marker removal + last-out termination).

    The lock file is '.'-prefixed, so `sessions_dir_empty` never counts it
    as a live session. On any flock failure this degrades to unlocked
    execution — the old race returns, but the lifecycle still works.
    """
    import errno
    import fcntl
    import time

    lock_path = Path(project_dir) / ".dut-serial" / "sessions" / ".lock"
    fd = None
    try:
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o666)
        deadline = time.monotonic() + timeout
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError as e:
                if e.errno not in (errno.EACCES, errno.EAGAIN):
                    fd = None
                    break
                if time.monotonic() >= deadline:
                    fd = None
                    break
                time.sleep(0.05)
        yield
    except OSError:
        yield
    finally:
        if fd is not None:
            try:
                fcntl.flock(fd, fcntl.LOCK_UN)
            except OSError:
                pass
            os.close(fd)


def sessions_dir_state(project_dir: str) -> Optional[bool]:
    """`True` when no live session markers remain, `False` when some do,
    `None` when the state is UNKNOWN (no sessions dir at all — e.g. a
    session that never wrote a marker may still be using the server, so
    "empty" would be a lie that kills a live consumer)."""
    sessions = Path(project_dir) / ".dut-serial" / "sessions"
    try:
        names = sessions.iterdir()
        return not any(not n.name.startswith(".") for n in names)
    except FileNotFoundError:
        return None
    except OSError:
        return False


def owner_session_marker_exists(project_dir: str, owner_session_id) -> bool:
    """Does the owning session still have a live marker? (Guest-takeover
    gate: a record claiming alive whose owner marker is GONE means the
    owner session vanished without running session-stop.)"""
    marker = session_marker_path(project_dir, owner_session_id)
    if marker is None:
        # No usable session id in the record → conservatively "present".
        return True
    return marker.is_file()


def terminate_pid(
    pid: int,
    grace_secs: float = 3.0,
    identity_check: Optional[Callable[[int], bool]] = None,
) -> bool:
    """SIGTERM a pid, wait up to `grace_secs`, SIGKILL as fallback.

    For LIFECYCLE paths that have already verified the pid's identity —
    never call with an unverified pid. `identity_check` (when given) is
    re-run immediately before EACH signal, closing the small pid-reuse
    window between an earlier verification and the kill. Returns True when
    the process is gone by return."""
    import signal
    import time as _time

    def _gone() -> bool:
        try:
            os.kill(pid, 0)
            return False
        except ProcessLookupError:
            return True
        except OSError:
            # EPERM etc.: exists but not ours to signal.
            return True

    def _signal(sig: int) -> bool:
        if identity_check is not None and not identity_check(pid):
            # The pid no longer matches the verified target — do not touch
            # whatever reused it.
            return True
        try:
            os.kill(pid, sig)
        except OSError:
            return True
        return False

    if _signal(signal.SIGTERM):
        return True
    deadline = _time.monotonic() + grace_secs
    while _time.monotonic() < deadline:
        if _gone():
            return True
        _time.sleep(0.1)
    _signal(signal.SIGKILL)
    _time.sleep(0.1)
    return _gone()


# ── Self-test ──────────────────────────────────────────────────────────────
# Run with: `uv run hooks/claude/lib.py`
# Asserts inventory-schema rendering parity: for each of the 8 Rust states
# (TargetState::as_str) the rendered output equals the EXACT ANSI strings
# StateManager::format_statusline_labeled produces today (sermcp/src/
# state_manager.rs:402). If the Rust side changes its formatting, this
# selftest is the drift detector — update RUST_STATELINE together with it.
if __name__ == "__main__":
    import json as _json
    import tempfile

    # Literal outputs of format_statusline_labeled — do NOT edit without
    # editing the Rust side in the same change.
    RUST_STATELINE = {
        "active": "\x1b[32m● {label}:active\x1b[0m",
        "booting": "\x1b[33m◐ {label}:booting\x1b[0m",
        "uboot": "\x1b[36m● {label}:uboot\x1b[0m",
        "crashed": "\x1b[31m✗ {label}:crashed\x1b[0m",
        "disconnected": "\x1b[31m✗ {label}:disconnected\x1b[0m",
        "DUT-off": "\x1b[31m✗ {label}:DUT-off\x1b[0m",
        "dutabo": "\x1b[35m● {label}:dutabo\x1b[0m",
        # stopped/unknown have no TargetState -> empty statusline text.
    }
    failed = 0

    def _check(name: str, cond: bool, detail: str = "") -> None:
        global failed
        if cond:
            print(f"OK    {name}")
        else:
            print(f"FAIL  {name}{(' — ' + detail) if detail else ''}")
            failed += 1

    # ── Rendering parity: state_text is rendered verbatim, never re-mapped.
    for state, fmt in RUST_STATELINE.items():
        expected = fmt.format(label="rk3576")
        dut = {"dut_name": "rk3576", "state": state, "state_text": expected}
        got = _inventory.render_state(dut)
        _check(
            f"state_text verbatim for {state}",
            got == expected,
            f"expected {expected!r}, got {got!r}",
        )
    for state in ("stopped", "unknown"):
        dut = {"dut_name": "rk3576", "state": state, "state_text": ""}
        _check(
            f"{state} renders nothing",
            _inventory.render_state(dut) == "",
        )

    # ── inventory_is_live: identity + freshness gates.
    # Identity: a bare python interpreter is NOT
    # the MCP server — comm/cmdline must identify sermcp — so the
    # self pid (this very test process) must be rejected even though it
    # is trivially alive.
    _check(
        "self pid (python3) is NOT a live MCP server (identity gate)",
        not _inventory.inventory_is_live({"mcp_pid": os.getpid()}),
    )
    _check(
        "nonexistent pid is dead",
        not _inventory.inventory_is_live({"mcp_pid": 99999999}),  # > Linux pid_max
    )
    _check(
        "missing/None pid is never live",
        not _inventory.inventory_is_live({})
        and not _inventory.inventory_is_live({"mcp_pid": None}),
    )

    # Freshness gate coverage: patch the identity check (a real MCP pid is
    # not available in a unit test) so ONLY the heartbeat logic varies.
    _real_pid_is_mcp = _inventory._pid_is_mcp_server
    _inventory._pid_is_mcp_server = lambda pid: True
    try:
        import time as _time

        now = int(_time.time())
        _check(
            "freshness gate: fresh written_at is live",
            _inventory.inventory_is_live(
                {"mcp_pid": os.getpid(), "heartbeat_secs": 30, "written_at_unix": now}
            ),
        )
        _check(
            "freshness gate: stale written_at (> 3× heartbeat) is dead",
            not _inventory.inventory_is_live(
                {"mcp_pid": os.getpid(), "heartbeat_secs": 30, "written_at_unix": now - 400}
            ),
        )
        _check(
            "freshness gate: missing written_at with heartbeat is dead",
            not _inventory.inventory_is_live({"mcp_pid": os.getpid(), "heartbeat_secs": 30}),
        )
        _check(
            "no heartbeat declared → identity gate alone",
            _inventory.inventory_is_live({"mcp_pid": os.getpid()}),
        )
    finally:
        _inventory._pid_is_mcp_server = _real_pid_is_mcp

    # ── load_inventory: shape-checked, never raises.
    with tempfile.TemporaryDirectory() as td:
        proj = Path(td) / "proj"
        inv_dir = proj / ".dut-serial"
        inv_dir.mkdir(parents=True)

        good = {
            "schema_version": 1,
            "written_by": "mcp-server",
            "written_at_unix": 0,
            "mcp_pid": os.getpid(),
            "duts": [],
            "dev_hosts": [],
        }
        (inv_dir / "inventory.json").write_text(_json.dumps(good))
        _check("load_inventory parses schema v1", _inventory.load_inventory(str(proj)) is not None)

        (inv_dir / "inventory.json").write_text("{not json")
        _check("load_inventory rejects malformed JSON", _inventory.load_inventory(str(proj)) is None)

        (inv_dir / "inventory.json").write_text(_json.dumps({"schema_version": 99}))
        _check("load_inventory rejects wrong schema_version", _inventory.load_inventory(str(proj)) is None)

        (inv_dir / "inventory.json").unlink()
        _check("load_inventory None when absent", _inventory.load_inventory(str(proj)) is None)

    # ── find_project_root: TARGET_CONF, walk-up detection.
    # The /dev/shm fallback is disabled for these tests: real sessions leave
    # claude-serial-*.lock files pointing at /tmp scratch projects (other
    # agents' smoke tests), which would make the walk-up assertions depend
    # on unrelated machine state.
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        proj = base / "nested" / "proj"
        proj.mkdir(parents=True)
        (proj / ".target.jsonc").write_text("{}")

        old_env = os.environ.pop("TARGET_EXCLUDE", None)
        real_glob = _inventory.glob.glob
        _inventory.glob.glob = lambda pattern: []  # silence lock fallback
        try:
            # CWD INSIDE the project (walk-up inspects ancestors, not children)
            (proj / "sub").mkdir()
            os.chdir(proj / "sub")
            root = _inventory.find_project_root()
            _check(
                "walk-up finds .target.jsonc",
                root == str(proj),
                f"got {root!r}",
            )

            os.environ["TARGET_CONF"] = str(proj / ".target.jsonc")
            root = _inventory.find_project_root()
            _check(
                "TARGET_CONF to .target.jsonc is honored",
                root == str(proj),
                f"got {root!r}",
            )

            os.environ["TARGET_CONF"] = "/nonexistent/.target.jsonc"
            root = _inventory.find_project_root()
            _check(
                "set-but-missing TARGET_CONF yields None (Rust errors loudly)",
                root is None,
                f"got {root!r}",
            )

            (proj / ".target.jsonc").unlink()
            os.environ.pop("TARGET_CONF", None)
            root = _inventory.find_project_root()
            _check(
                "no config anywhere yields None",
                root is None,
                f"got {root!r}",
            )

            # ── Lock fallback gating: the display path must never leak
            #    another project's DUT state (walk-up + TARGET_CONF only). ──
            foreign = base / "foreign-proj"
            foreign.mkdir(parents=True)
            (foreign / ".target.jsonc").write_text("{}")
            lock_path = base / "claude-serial-ffffffff.lock"
            lock_path.write_text(str(foreign))

            def _fake_glob(pattern):
                return [str(lock_path)]

            _inventory.glob.glob = _fake_glob
            try:
                root = _inventory.find_project_root()
                _check(
                    "lock fallback ON returns the recorded project",
                    root == str(foreign),
                    f"got {root!r}",
                )
                root = _inventory.find_project_root(allow_lock_fallback=False)
                _check(
                    "display path (fallback OFF) never leaks a foreign project",
                    root is None,
                    f"got {root!r}",
                )
            finally:
                _inventory.glob.glob = real_glob
        finally:
            _inventory.glob.glob = real_glob
            os.chdir(base)
            if old_env is not None:
                os.environ["TARGET_EXCLUDE"] = old_env
            os.environ.pop("TARGET_CONF", None)

    # ── P6: the /dev/shm lock fallback is SHIPPED,
    #    OPT-IN behavior — session-lifecycle paths keep the default (locks
    #    consulted as a last resort), display paths must pass
    #    allow_lock_fallback=False. The previous P6 block asserted an API
    #    that was never shipped (a start_dir parameter with the fallback
    #    deleted); those assertions are realigned to the code above.
    import inspect as _inspect
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        neutral = base / "neutral"
        foreign = base / "foreign"
        neutral.mkdir()
        foreign.mkdir()
        (foreign / ".target.jsonc").write_text("{}")
        scratch_lock = base / "claude-serial-ffffffff.lock"
        scratch_lock.write_text(str(foreign))

        old_conf = os.environ.pop("TARGET_CONF", None)
        old_excl = os.environ.pop("TARGET_EXCLUDE", None)
        real_glob = _inventory.glob.glob
        _inventory.glob.glob = lambda pattern: [str(scratch_lock)]
        try:
            os.chdir(neutral)
            root = _inventory.find_project_root()
            _check(
                "P6 lifecycle default (fallback ON) resolves the lock's project",
                root == str(foreign),
                f"got {root!r}",
            )
            root = _inventory.find_project_root(allow_lock_fallback=False)
            _check(
                "P6 display path (fallback OFF) never consults locks",
                root is None,
                f"got {root!r}",
            )
            params = _inspect.signature(_inventory.find_project_root).parameters
            _check(
                "P6 allow_lock_fallback defaults to True (opt-out)",
                params.get("allow_lock_fallback") is not None
                and params["allow_lock_fallback"].default is True,
                f"params: {sorted(params)}",
            )
        finally:
            _inventory.glob.glob = real_glob
            os.chdir(base)
            if old_conf is not None:
                os.environ["TARGET_CONF"] = old_conf
            else:
                os.environ.pop("TARGET_CONF", None)
            if old_excl is not None:
                os.environ["TARGET_EXCLUDE"] = old_excl
            else:
                os.environ.pop("TARGET_EXCLUDE", None)

    # ── Lifecycle helpers: /health probe (stdlib-only, never raises).
    _check(
        "http_health_ok is False on a closed port",
        not http_health_ok(1),  # port 1: nothing listens, refused instantly
    )
    _check(
        "http_health_ok rejects non-ports",
        not http_health_ok(None) and not http_health_ok(0) and not http_health_ok(70000),
    )
    import threading as _threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    class _HealthHandler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/health":
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")
            else:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()

        def log_message(self, *args):
            pass

    _srv = HTTPServer(("127.0.0.1", 0), _HealthHandler)
    _srv_port = _srv.server_port
    _th = _threading.Thread(target=_srv.serve_forever, daemon=True)
    _th.start()
    try:
        _check("http_health_ok True on a healthy /health", http_health_ok(_srv_port, timeout=2.0))
    finally:
        _srv.shutdown()
        _srv.server_close()

    # ── Session markers: atomic write, role content, removal, empty check,
    #    and the guest-takeover gate.
    with tempfile.TemporaryDirectory() as td:
        proj = Path(td) / "proj"
        (proj / ".dut-serial").mkdir(parents=True)

        # No sessions dir at all = UNKNOWN (a marker-less consumer — dutabo,
        # dsh — may still be using the server): session-stop must NOT kill.
        _check("no marker dir yet → sessions state is UNKNOWN", sessions_dir_state(str(proj)) is None)
        mark_session_alive(str(proj), "sess-uuid-1", "owner")
        mark_session_alive(str(proj), "sess-uuid-2", "guest")
        m1 = session_marker_path(str(proj), "sess-uuid-1")
        m2 = session_marker_path(str(proj), "sess-uuid-2")
        _check("markers exist under .dut-serial/sessions/", m1 is not None and m1.is_file() and m2.is_file())
        lines = m1.read_text().splitlines()
        _check(
            "marker holds ISO timestamp + role",
            len(lines) == 2 and lines[1] == "owner" and "T" in lines[0],
            m1.read_text(),
        )
        _check("two live sessions → not empty", sessions_dir_state(str(proj)) is False)
        _check("owner marker presence gate", owner_session_marker_exists(str(proj), "sess-uuid-1"))
        _check(
            "owner marker gone when session-stop removed it",
            (remove_session_marker(str(proj), "sess-uuid-1") and not owner_session_marker_exists(str(proj), "sess-uuid-1")),
        )
        _check("removing an absent marker is a no-op", not remove_session_marker(str(proj), "sess-uuid-1"))
        # Crash remnant tmp files must not keep the dir "occupied".
        (m2.parent / ".sess-uuid-2.tmp-123").write_text("garbage")
        _check(
            "last marker out (only tmp remnants left) → sessions dir empty",
            remove_session_marker(str(proj), "sess-uuid-2") and sessions_dir_state(str(proj)) is True,
        )
        _check(
            "hostile session id stays one path component",
            (session_marker_path(str(proj), "../evil") is not None
             and session_marker_path(str(proj), "../evil").parent.name == "sessions"),
        )

    raise SystemExit(0 if failed == 0 else 1)
