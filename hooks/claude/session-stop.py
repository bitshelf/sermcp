#!/usr/bin/env python3
"""SessionStop hook — release ownership, session markers, and (last one
out) this project's MCP servers.

Lifecycle contract: the HTTP MCP servers self-terminate after
their idle timeout (~10min without MCP/WebSocket activity) — that is the
BACKSTOP for crashed clients. This hook provides the prompt path:

  1. removes THIS session's marker from `.dut-serial/sessions/`
     (every session-start wrote one — owner AND guests);
  2. when the sessions dir is now empty (this was the LAST session),
     terminates this project's MCP servers: candidate pids come from
     `.dut-serial/mcp.pid`, `.dut-serial/*/mcp.pid` (per-DUT layout) and
     inventory.json's mcp_pid; each is killed ONLY after /proc identity
     (comm or argv[0] contains "sermcp") AND project anchoring
     (cwd == project root, or TARGET_CONF in its environ points inside
     the project) verify it. SIGTERM, 3s grace, SIGKILL. A pid that
     cannot be verified is NEVER killed;
  3. releases the project ownership record if THIS session is the owner
     (first-opens-wins; the next live agent re-claims on demand);
  4. cleans the CWD-keyed git cache lock dir.

Without a session_id in the hook input this cleans conservatively: no
marker removal, no kills (the idle timeout mops up), just a stderr note.
No /dev/shm cross-project fallback; no blind PID kills (the old
.session-pid scheme could SIGTERM an unrelated process after PID reuse).
"""

import os
import sys
import hashlib
import json
from pathlib import Path

_HOOK_DIR = Path(__file__).resolve().parent
if str(_HOOK_DIR) not in sys.path:
    sys.path.insert(0, str(_HOOK_DIR))

from inventory import load_inventory
from lib import (
    _is_embedded_server,
    find_project_dir,
    pid_anchored_to_project,
    read_hook_payload,
    remove_session_marker,
    sessions_dir_state,
    sessions_lock,
    terminate_pid,
)
from owner import release_owner_for_session

# Shared implementation (moved to lib so the user-prompt-submit restart
# path can anchor its kills identically). The underscore alias keeps the
# selftest's monkey-patching surface unchanged.
_pid_anchored_to_project = pid_anchored_to_project


def _candidate_server_pids(project_dir: str) -> list:
    """Collect candidate pids for THIS project from the per-DUT pidfiles
    (`.dut-serial/*/mcp.pid`), the root `.dut-serial/mcp.pid`, and
    inventory.json's mcp_pid. Deduplicated; no verification yet."""
    proj = Path(project_dir) / ".dut-serial"
    candidates = []

    def _read_pid(path: Path):
        try:
            pid = int(path.read_text().strip())
        except (ValueError, OSError):
            return None
        return pid if pid > 0 else None

    root_pid = _read_pid(proj / "mcp.pid")
    if root_pid:
        candidates.append(root_pid)
    try:
        for entry in sorted(proj.iterdir()):
            pid = _read_pid(entry / "mcp.pid")
            if pid:
                candidates.append(pid)
    except OSError:
        pass
    inv = load_inventory(project_dir)
    if inv is not None and isinstance(inv.get("mcp_pid"), int) and inv["mcp_pid"] > 0:
        candidates.append(inv["mcp_pid"])

    seen = set()
    unique = []
    for pid in candidates:
        if pid not in seen and pid != os.getpid():
            seen.add(pid)
            unique.append(pid)
    return unique


def _terminate_project_servers(project_dir: str) -> list:
    """Kill every VERIFIED server of this project (identity + anchoring).
    Returns the killed pids. Never raises, never kills what it could not
    verify both ways."""
    killed = []
    for pid in _candidate_server_pids(project_dir):
        if not _is_embedded_server(pid):
            continue
        if not _pid_anchored_to_project(pid, project_dir):
            continue
        print(
            f"[sermcp] last session out — stopping MCP server pid {pid} "
            f"for {project_dir}",
            file=sys.stderr,
        )
        # identity_check re-verifies anchoring right before each signal,
        # closing the pid-reuse window between the check above and the kill.
        if terminate_pid(
            pid,
            grace_secs=3.0,
            identity_check=lambda p: _is_embedded_server(p)
            and _pid_anchored_to_project(p, project_dir),
        ):
            killed.append(pid)
    # Drop pidfiles that pointed at the servers we just killed.
    proj = Path(project_dir) / ".dut-serial"
    for killed_pid in killed:
        try:
            entries = [proj] + (sorted(proj.iterdir()) if proj.is_dir() else [])
            for entry in entries:
                pid_file = entry / "mcp.pid"
                if pid_file.is_file() and pid_file.read_text().strip() == str(killed_pid):
                    pid_file.unlink()
        except OSError:
            pass
    return killed


def main():
    session_id = read_hook_payload().get("session_id")

    # Display/cleanup path: no cross-project fallback — never touch another
    # project's locks or state.
    project_dir = find_project_dir(allow_lock_fallback=False)
    if project_dir:
        if session_id is None:
            # Conservative: without a session id we cannot know whether
            # OTHER sessions still use the servers — leave everything to
            # the server's own idle timeout.
            print(
                "[sermcp] session-stop: no session_id in hook input — "
                "skipping marker removal and MCP termination",
                file=sys.stderr,
            )
        else:
            # Last session out → this project's servers are no longer
            # needed (a future session-start respawns on demand).
            # Serialized against concurrent session-starts: without the
            # lock, an empty-check can pass just before another session's
            # marker lands, and the fresh session's server dies with no one
            # left to respawn it. Kill ONLY when this session actually
            # removed a marker (proves it was counted) and the directory
            # state is KNOWN-empty — a missing dir means marker-less
            # consumers (dutabo, dsh) may still be using the server.
            with sessions_lock(project_dir):
                removed = remove_session_marker(project_dir, session_id)
                if removed and sessions_dir_state(project_dir) is True:
                    _terminate_project_servers(project_dir)

        # Ownership release is idempotent and session-scoped: only clears
        # the record when it still belongs to this session id.
        release_owner_for_session(project_dir, session_id)

    # Clean git cache lock dir — CWD-keyed to match statusline.py's
    # _git_cache_path() (keys on the resolved CWD since the cross-session
    # cache-poisoning fix).
    cache_root = "/dev/shm" if os.path.isdir("/dev/shm") and os.access("/dev/shm", os.W_OK) else os.environ.get("TMPDIR", "/tmp")
    cwd_h = hashlib.md5(str(Path.cwd().resolve()).encode()).hexdigest()[:8]
    lock_dir = Path(f"{cache_root}/claude-{cwd_h}-git.cache.lock")
    try:
        if lock_dir.is_dir():
            lock_dir.rmdir()
    except OSError:
        pass

    sys.exit(0)


def _selftest() -> int:
    """Session-stop lifecycle checks.

    Run with: `python3 hooks/claude/session-stop.py --selftest`
    - removes THIS session's marker from .dut-serial/sessions/;
    - kills this project's servers ONLY when the last marker is out, and
      only pids that pass identity AND project anchoring;
    - an unverified / foreign-anchored pid is never killed;
    - without a session_id: conservative no-op (no marker ops, no kills);
    - releases owner.json only for the own session id.
    (The old S1-S7 block asserted a "session.lock" API that was never
    shipped — it was replaced by owner.json; see hooks/claude/owner.py.)
    """
    import io
    import json as _json
    import contextlib
    import tempfile

    failed = 0

    def check(name, cond, detail=""):
        nonlocal failed
        if cond:
            print(f"OK    {name}")
        else:
            print(f"FAIL  {name}{(' — ' + detail) if detail else ''}")
            failed += 1

    def _run_main(payload):
        real_stdin = sys.stdin
        buf = io.StringIO()
        try:
            sys.stdin = io.StringIO(_json.dumps(payload))
            with contextlib.redirect_stdout(buf):
                try:
                    main()
                except SystemExit:
                    pass
        finally:
            sys.stdin = real_stdin
        return buf.getvalue()

    def _marker(proj, sid, role="guest"):
        (proj / ".dut-serial" / "sessions").mkdir(parents=True, exist_ok=True)
        (proj / ".dut-serial" / "sessions" / sid).write_text(f"2026-01-01T00:00:00+00:00\n{role}\n")

    def _clear_sessions(proj):
        import shutil

        shutil.rmtree(proj / ".dut-serial" / "sessions", ignore_errors=True)

    old_cwd = os.getcwd()
    old_conf = os.environ.pop("TARGET_CONF", None)

    # Patchable seams for the kill path (module-level names, resolved at
    # call time inside _terminate_project_servers).
    real_candidates = globals().get("_candidate_server_pids")
    real_embedded = globals().get("_is_embedded_server")
    real_anchored = globals().get("_pid_anchored_to_project")
    real_terminate = globals().get("terminate_pid")

    kills = []

    def _record_terminate(pid, grace_secs=3.0, identity_check=None):
        kills.append(pid)
        return True

    try:
        with tempfile.TemporaryDirectory() as td:
            base = Path(td)
            proj = base / "proj"
            (proj / ".dut-serial").mkdir(parents=True)
            (proj / ".target.jsonc").write_text("{}")
            os.chdir(proj)

            # ── K1: last session out → verified server pids terminated.
            _marker(proj, "me")
            globals()["_candidate_server_pids"] = lambda project_dir: [424242]
            globals()["_is_embedded_server"] = lambda pid: True
            globals()["_pid_anchored_to_project"] = lambda pid, project_dir: True
            globals()["terminate_pid"] = _record_terminate
            try:
                kills.clear()
                _run_main({"session_id": "me"})
                check(
                    "K1 last session out terminates the verified server pid",
                    kills == [424242],
                    f"kills: {kills!r}",
                )
                check(
                    "K1 the session marker is removed",
                    not (proj / ".dut-serial" / "sessions" / "me").exists(),
                )

                # ── K2: another session's marker still present → no kill.
                _marker(proj, "other")
                kills.clear()
                _run_main({"session_id": "me"})
                check(
                    "K2 other sessions still live → no kill",
                    kills == [] and (proj / ".dut-serial" / "sessions" / "other").exists(),
                    f"kills: {kills!r}",
                )

                # ── K3: identity fails → never killed (kill path IS reached:
                #    sessions dir is empty, so only the gate protects 424242).
                _clear_sessions(proj)
                _marker(proj, "me")
                globals()["_is_embedded_server"] = lambda pid: False
                kills.clear()
                _run_main({"session_id": "me"})
                check("K3 unverified (non-sermcp) pid is never killed", kills == [])

                # ── K4: anchored to ANOTHER project → never killed.
                globals()["_is_embedded_server"] = lambda pid: True
                globals()["_pid_anchored_to_project"] = lambda pid, project_dir: False
                _clear_sessions(proj)
                _marker(proj, "me")
                kills.clear()
                _run_main({"session_id": "me"})
                check("K4 foreign-anchored pid is never killed", kills == [])

                # ── K5: no session_id → conservative no-op.
                globals()["_pid_anchored_to_project"] = lambda pid, project_dir: True
                _marker(proj, "me")
                kills.clear()
                _run_main({})
                check(
                    "K5 no session_id → no kill, marker left alone",
                    kills == [] and (proj / ".dut-serial" / "sessions" / "me").exists(),
                    f"kills: {kills!r}",
                )
            finally:
                if real_candidates is not None:
                    globals()["_candidate_server_pids"] = real_candidates
                else:
                    globals().pop("_candidate_server_pids", None)
                if real_embedded is not None:
                    globals()["_is_embedded_server"] = real_embedded
                else:
                    globals().pop("_is_embedded_server", None)
                if real_anchored is not None:
                    globals()["_pid_anchored_to_project"] = real_anchored
                else:
                    globals().pop("_pid_anchored_to_project", None)
                if real_terminate is not None:
                    globals()["terminate_pid"] = real_terminate
                else:
                    globals().pop("terminate_pid", None)

            # ── O1/O2: owner.json release is session-scoped (main path).
            import json as _dump

            owner_file = proj / ".dut-serial" / "owner.json"
            owner_file.write_text(_dump.dumps({
                "schema_version": 1, "owner_kind": "agent", "owner_pid": 1111,
                "owner_session_id": "other", "owner_mcp_pid": None,
                "owner_cwd": str(proj), "claimed_at_unix": 0,
            }))
            _run_main({"session_id": "me"})
            check("O1 foreign owner record survives", owner_file.exists())

            owner_file.write_text(_dump.dumps({
                "schema_version": 1, "owner_kind": "agent", "owner_pid": 1111,
                "owner_session_id": "me", "owner_mcp_pid": None,
                "owner_cwd": str(proj), "claimed_at_unix": 0,
            }))
            _marker(proj, "me2")  # keep a marker so the kill path stays inert
            _run_main({"session_id": "me"})
            check("O2 own owner record is released", not owner_file.exists())

            # ── C1: candidate collection honors the per-DUT pidfile layout
            #    and the inventory pid.
            (proj / ".dut-serial" / "dut-a").mkdir(exist_ok=True)
            (proj / ".dut-serial" / "dut-a" / "mcp.pid").write_text("111")
            (proj / ".dut-serial" / "dut-b").mkdir(exist_ok=True)
            (proj / ".dut-serial" / "dut-b" / "mcp.pid").write_text("222")
            (proj / ".dut-serial" / "mcp.pid").write_text("111")  # dup → once
            (proj / ".dut-serial" / "inventory.json").write_text(_dump.dumps({
                "schema_version": 1, "mcp_pid": 333,
            }))
            check(
                "C1 candidates = per-DUT pidfiles + root + inventory, deduped",
                sorted(_candidate_server_pids(str(proj))) == [111, 222, 333],
                f"got {_candidate_server_pids(str(proj))!r}",
            )
    finally:
        os.chdir(old_cwd)
        if old_conf is not None:
            os.environ["TARGET_CONF"] = old_conf
        else:
            os.environ.pop("TARGET_CONF", None)

    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(_selftest())
    main()
