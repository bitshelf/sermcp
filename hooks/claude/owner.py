#!/usr/bin/env python3
"""Project ownership record — first-opens-wins with liveness following.

Multiple code agents on one machine may open the same project. The FIRST
agent to claim owns the MCP lifecycle (start/restart/stop) and the serial
console; later agents connect only (guests) and their statuslines show an
ownership marker. Ownership follows liveness: the record anchors on the
serving MCP pid (`owner_mcp_pid`), so when the owner's server dies the
record is treated as vacant and the next live agent to check re-claims —
the MCP can then be restarted by the new owner.

`dutabo serial` never changes agent ownership: its takeover is already
visible to everyone through the `dutabo` DUT state.

Record: `<project>/.dut-serial/owner.json` (schema_version 1):

    {
      "schema_version": 1,
      "owner_kind": "agent",
      "owner_pid": <claiming process pid, informational>,
      "owner_session_id": "<agent session id or null>",
      "owner_mcp_pid": <serving MCP pid or null — the liveness anchor>,
      "owner_cwd": "<absolute project dir>",
      "claimed_at_unix": <ts>
    }

Stdlib only; every function is a file read/write + os.kill, never a
subprocess, so the statusline hot path stays cheap.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Optional

OWNER_FILE = "owner.json"
OWNER_LOCK_FILE = "owner.lock"
SCHEMA_VERSION = 1
# A claim lock older than this is a crash remnant and may be reclaimed.
STALE_LOCK_SECS = 10


def owner_path(project_dir: str) -> Path:
    return Path(project_dir) / ".dut-serial" / OWNER_FILE


def pid_alive(pid) -> bool:
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def load_owner(project_dir: str) -> Optional[dict]:
    """Read and shape-check the owner record; None when absent/invalid."""
    try:
        obj = json.loads(owner_path(project_dir).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(obj, dict) or obj.get("schema_version") != SCHEMA_VERSION:
        return None
    return obj


# A claim whose MCP anchor is not yet written is treated as alive for this
# long (session-start claims before Claude Code spawns the stdio server).
CLAIM_GRACE_SECS = 60


def owner_is_alive(owner: Optional[dict]) -> bool:
    """Liveness anchored on the serving MCP pid (see module docstring).

    A fresh claim with no anchor pid yet is alive for `CLAIM_GRACE_SECS` —
    the hook claims before the MCP spawns and the server back-fills the
    anchor on its first inventory refresh.
    """
    if not isinstance(owner, dict):
        return False
    if pid_alive(owner.get("owner_mcp_pid")):
        return True
    if owner.get("owner_mcp_pid") is None:
        claimed = owner.get("claimed_at_unix")
        if isinstance(claimed, (int, float)):
            return time.time() - claimed < CLAIM_GRACE_SECS
    return False


def am_i_owner(owner: Optional[dict], session_id: Optional[str]) -> bool:
    """Whether the given session id matches the (live) record's owner."""
    if not owner_is_alive(owner):
        return False
    if session_id is None or not isinstance(owner.get("owner_session_id"), str):
        return False
    return owner.get("owner_session_id") == session_id


def _acquire_claim_lock(dut_serial: Path) -> bool:
    """Exclusive-create the claim lock; removes crash remnants first.

    Returns True when THIS caller owns the lock. On contention the caller
    just waits below — the lock only serializes the read-check-write
    sequence, never blocks on anything slow.
    """
    lock = dut_serial / OWNER_LOCK_FILE
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        os.close(fd)
        return True
    except FileExistsError:
        try:
            if time.time() - lock.stat().st_mtime > STALE_LOCK_SECS:
                lock.unlink()
                fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
                os.close(fd)
                return True
        except OSError:
            pass
        return False


def claim_owner(
    project_dir: str,
    owner_pid: int,
    owner_session_id: Optional[str],
    owner_mcp_pid: Optional[int] = None,
) -> dict:
    """Claim (or refresh) project ownership; first-live-writer wins.

    Returns the authoritative record after the call — either the existing
    live owner's record (this caller is a guest) or the freshly written one
    (this caller is now the owner). A record whose anchor MCP pid is dead
    is vacant: the next caller to notice re-claims it.
    """
    dut_serial = Path(project_dir) / ".dut-serial"
    dut_serial.mkdir(parents=True, exist_ok=True)
    acquired = _acquire_claim_lock(dut_serial)
    if not acquired:
        # Another claimer is writing right now — wait for its result.
        deadline = time.time() + 2.0
        while time.time() < deadline:
            rec = load_owner(project_dir)
            if rec is not None:
                return rec
            time.sleep(0.05)
        acquired = _acquire_claim_lock(dut_serial)
    try:
        rec = load_owner(project_dir)
        if rec is not None and owner_is_alive(rec):
            return rec
        record = {
            "schema_version": SCHEMA_VERSION,
            "owner_kind": "agent",
            "owner_pid": int(owner_pid),
            "owner_session_id": owner_session_id,
            "owner_mcp_pid": owner_mcp_pid,
            "owner_cwd": str(Path(project_dir).resolve()),
            "claimed_at_unix": int(time.time()),
        }
        path = owner_path(project_dir)
        tmp = path.with_name(f".{OWNER_FILE}.tmp-{os.getpid()}")
        tmp.write_text(json.dumps(record))
        os.replace(tmp, path)
        return record
    finally:
        if acquired:
            try:
                (dut_serial / OWNER_LOCK_FILE).unlink()
            except OSError:
                pass


def release_owner(project_dir: str, owner_pid: int) -> None:
    """Release ownership ONLY if the record still belongs to this pid."""
    try:
        rec = load_owner(project_dir)
    except OSError:
        return
    if rec is None or rec.get("owner_pid") != int(owner_pid):
        return
    try:
        owner_path(project_dir).unlink()
    except OSError:
        pass


def release_owner_for_session(project_dir: str, session_id: Optional[str]) -> None:
    """Release ownership ONLY if the record belongs to this session id."""
    if session_id is None:
        return
    try:
        rec = load_owner(project_dir)
    except OSError:
        return
    if rec is None or rec.get("owner_session_id") != session_id:
        return
    try:
        owner_path(project_dir).unlink()
    except OSError:
        pass


def refresh_owner_mcp_pid(project_dir: str, session_id: Optional[str], mcp_pid: int) -> None:
    """Update the record's liveness anchor, only while we still own it."""
    if session_id is None:
        return
    rec = load_owner(project_dir)
    if rec is None or rec.get("owner_session_id") != session_id:
        return
    rec["owner_mcp_pid"] = int(mcp_pid)
    path = owner_path(project_dir)
    try:
        tmp = path.with_name(f".{OWNER_FILE}.tmp-{os.getpid()}")
        tmp.write_text(json.dumps(rec))
        os.replace(tmp, path)
    except OSError:
        pass


def _selftest() -> int:
    """Claim-protocol regression checks. Run: python3 hooks/claude/owner.py --selftest

    - first-live-writer wins: a second claimer gets the existing record;
    - re-claim when the anchor pid is dead;
    - grace window keeps a not-yet-anchored fresh claim alive;
    - release only clears the record when the session id matches.
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
        proj = os.path.join(td, "proj")
        os.makedirs(os.path.join(proj, ".dut-serial"))

        rec = claim_owner(proj, 1111, "sess-a", owner_mcp_pid=None)
        check("first claimer becomes owner", rec.get("owner_session_id") == "sess-a")
        check("fresh claim stays alive (grace)", owner_is_alive(rec))

        rec2 = claim_owner(proj, 2222, "sess-b", owner_mcp_pid=None)
        check("second claimer is a guest", rec2.get("owner_session_id") == "sess-a")
        check("guest's am_i_owner is False", not am_i_owner(rec2, "sess-b"))
        check("owner's am_i_owner is True", am_i_owner(rec2, "sess-a"))

        # Anchor dies → vacant → next claimer takes over. (Write a record
        # with a DEAD anchor directly: a claim-created record is still inside
        # its grace window and would correctly stay alive.)
        owner_path(proj).write_text(json.dumps({
            "schema_version": 1, "owner_kind": "agent", "owner_pid": 1111,
            "owner_session_id": "sess-a", "owner_mcp_pid": 1 << 30,
            "owner_cwd": proj, "claimed_at_unix": int(time.time()),
        }))
        rec3 = claim_owner(proj, 2222, "sess-b", owner_mcp_pid=None)
        check("dead anchor vacates the record", rec3.get("owner_session_id") == "sess-b")

        release_owner_for_session(proj, "sess-a")
        check("foreign session cannot release", load_owner(proj) is not None)
        release_owner_for_session(proj, "sess-b")
        check("owner session releases the record", load_owner(proj) is None)

    return 1 if failed else 0


if __name__ == "__main__":
    import sys

    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(_selftest())
