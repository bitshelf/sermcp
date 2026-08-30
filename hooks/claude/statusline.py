#!/usr/bin/env python3
"""
Statusline hook — reads Rust-written inventory, formats git branch + model.

Architecture (inventory.json, zero config parsing):
  MCP Server ──atomic_write──→ .dut-serial/inventory.json
  Statusline hook reads inventory (<1ms), gates on the recorded mcp_pid via
  os.kill(pid, 0), and renders each DUT's pre-formatted ANSI `state_text`
  verbatim — the state -> color mapping lives ONLY in state_manager.rs, so
  the two can never drift. No daemon, no inotify, no network I/O, and NO
  subprocess on the serial hot path (enforced by the --selftest stub).
"""

import os
import sys
import json
import time
from pathlib import Path

_HOOK_DIR = Path(__file__).resolve().parent
if str(_HOOK_DIR) not in sys.path:
    sys.path.insert(0, str(_HOOK_DIR))

from lib import find_project_dir
from inventory import _is_excluded, load_inventory, inventory_is_live, render_state
from owner import load_owner, owner_is_alive, am_i_owner

# ── Config ──────────────────────────────────────────────────────────────────
TMP_ROOT = os.environ.get("TMPDIR", os.environ.get("TMP", "/tmp"))
CACHE_DIR = "/dev/shm" if os.path.isdir("/dev/shm") and os.access("/dev/shm", os.W_OK) else TMP_ROOT
CACHE_TTL = 10


# ── Git branch (cached, async refresh) ─────────────────────────────────────

def _git_cache_path() -> str:
    """Key the git cache on the CWD, not on find_project_dir().

    Sessions whose CWD has no config fall back to a shared /dev/shm lock
    project, so keying on the project made unrelated sessions share one
    cache file — the last writer's branch (e.g. "LangShen" from a kernel
    tree) then rendered in every other session (e.g. ubuntu-arm64-cross).
    The displayed branch is a property of the repo the user is in, so key
    on the resolved CWD.
    """
    import hashlib
    h = hashlib.md5(str(Path.cwd().resolve()).encode()).hexdigest()[:8]
    return f"{CACHE_DIR}/claude-{h}-git.cache"


def _read_git_cache():
    cache = _git_cache_path()
    try:
        with open(cache, "r") as f:
            ts_str, branch = f.readline().strip().split(" ", 1)
            if time.time() - float(ts_str) < CACHE_TTL:
                return branch
    except (FileNotFoundError, ValueError, OSError):
        return None
    # Expired: self-reap so stale cache files never accumulate.
    try:
        os.unlink(cache)
    except OSError:
        pass
    return None


def _reap_git_cache_lock(lock_dir) -> bool:
    """Remove a git-cache lock dir whose writer is dead.

    The writer stamps its pid in `owner.pid` inside the lock dir. A lock
    with a LIVE writer is never touched (regardless of age); one whose
    writer died is reclaimed immediately — no waiting out the freshness
    window. Legacy lock dirs without an owner.pid stay to the age-based
    fallback (returned False).
    """
    owner_file = Path(lock_dir) / "owner.pid"
    try:
        pid = int(owner_file.read_text().strip())
    except (OSError, ValueError):
        return False
    try:
        os.kill(pid, 0)
        return False
    except (OSError, ProcessLookupError):
        pass
    try:
        owner_file.unlink()
        Path(lock_dir).rmdir()
        return True
    except OSError:
        return False


def _refresh_git_cache_async() -> None:
    cache = _git_cache_path()
    lock_dir = cache + ".lock"
    if os.path.isdir(lock_dir):
        if not _reap_git_cache_lock(lock_dir):
            try:
                if time.time() - os.path.getmtime(lock_dir) < 15:
                    return
                os.rmdir(lock_dir)
            except OSError:
                return
    try:
        os.mkdir(lock_dir)
        # Stamp the writer so a later invocation can reclaim this lock the
        # moment this process dies instead of waiting out the age window.
        with open(os.path.join(lock_dir, "owner.pid"), "w") as f:
            f.write(str(os.getpid()))
    except FileExistsError:
        return
    except OSError:
        return
    try:
        # Use subprocess with cwd and argument list — no shell injection.
        # The script writes the result to the cache file atomically.
        import subprocess
        script = (
            'BRANCH=$(git branch --show-current 2>/dev/null || git rev-parse --short HEAD 2>/dev/null || echo "?"); '
            'git diff --quiet 2>/dev/null && git diff --cached --quiet 2>/dev/null || BRANCH="$BRANCH*"; '
            'echo "$(date +%s) $BRANCH"'
        )
        proc = subprocess.Popen(
            ["bash", "-c", script],
            cwd=os.getcwd(),
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )

        def _finish():
            try:
                out, _ = proc.communicate(timeout=5)
                if out:
                    with open(cache, "w") as f:
                        f.write(out.decode().strip())
            except (subprocess.TimeoutExpired, OSError):
                pass
            finally:
                try:
                    os.unlink(os.path.join(lock_dir, "owner.pid"))
                except OSError:
                    pass
                try:
                    os.rmdir(lock_dir)
                except OSError:
                    pass

        # Run the finish in a thread so we don't block the hook.
        import threading
        threading.Thread(target=_finish, daemon=True).start()
    except OSError:
        try:
            os.unlink(os.path.join(lock_dir, "owner.pid"))
            os.rmdir(lock_dir)
        except OSError:
            pass


def _is_git_repo() -> bool:
    return Path(".git").exists()


def _repo_manifest_branch() -> str | None:
    """Return the repo manifest branch when CWD is a repo-managed SDK root.

    CWD-only check (user rule: no walk-up of .git or .repo): .repo/ must
    exist at the CWD itself. Reads .repo/manifest.xml — following a
    symlink or an <include> stub — and extracts the
    <default revision="..."> value. Falls back to the .repo/manifests
    checkout branch read from its HEAD file (plain text, no subprocess),
    and finally to None (caller shows the basename). Never touches the
    CWD-keyed git cache.
    """
    repo_dir = Path(".repo")
    if not repo_dir.is_dir():
        return None
    branch = _parse_manifest_default_revision(repo_dir)
    if branch:
        return branch
    return _repo_manifests_head_branch(repo_dir)


def _parse_manifest_default_revision(repo_dir: Path) -> str | None:
    """Extract the <default revision="..."> value from .repo/manifest.xml."""
    import re
    manifest = repo_dir / "manifest.xml"
    if not manifest.is_file():
        return None
    try:
        content = manifest.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    # Rockchip-style stub: <include name="default.xml" /> ->
    # .repo/manifests/<name>. Symlinked manifests are followed by read.
    m = re.search(r'<include\b[^>]*name="([^"]+)"', content)
    if m:
        included = repo_dir / "manifests" / m.group(1)
        if included.is_file():
            try:
                content = included.read_text(encoding="utf-8", errors="replace")
            except OSError:
                return None
    # [^>] also matches newlines, so multi-line <default> elements work.
    m = re.search(r'<default\b[^>]*revision="([^"]+)"', content)
    if not m:
        return None
    revision = m.group(1)
    if revision.startswith("refs/heads/"):
        revision = revision[len("refs/heads/"):]
    return revision or None


def _repo_manifests_head_branch(repo_dir: Path) -> str | None:
    """Read the manifests checkout branch from its HEAD file (no subprocess).

    .repo/manifests/.git is normally a symlink to ../manifests.git and
    Path follows it. Detached HEAD (raw sha) or a non-branch ref -> None.
    """
    head = repo_dir / "manifests" / ".git" / "HEAD"
    if not head.is_file():
        return None
    try:
        ref = head.read_text(encoding="utf-8", errors="replace").strip()
    except OSError:
        return None
    if ref.startswith("ref: "):
        ref = ref[len("ref: "):].strip()
        if ref.startswith("refs/heads/"):
            return ref[len("refs/heads/"):]
    return None


def _compute_git_branch() -> str:
    import subprocess
    try:
        r = subprocess.run(
            ["git", "branch", "--show-current"],
            capture_output=True, text=True, timeout=1)
        branch = r.stdout.strip()
        if not branch:
            r = subprocess.run(
                ["git", "rev-parse", "--short", "HEAD"],
                capture_output=True, text=True, timeout=1)
            branch = r.stdout.strip() or "?"
        for flag in [["git", "diff", "--quiet"], ["git", "diff", "--cached", "--quiet"]]:
            r2 = subprocess.run(flag, capture_output=True, timeout=1)
            if r2.returncode != 0:
                branch += "*"
                break
        return branch
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        return "?"


def _write_git_cache(branch: str) -> None:
    cache = _git_cache_path()
    try:
        with open(cache, "w") as f:
            f.write(f"{time.time()} {branch}")
    except OSError:
        pass


def _get_git_branch() -> str:
    if not _is_git_repo():
        # CWD-only repo probe (user rule: no walk-up of .git or .repo).
        # Git wins when both .git and .repo exist at the CWD.
        manifest_branch = _repo_manifest_branch()
        if manifest_branch:
            return manifest_branch
        return os.path.basename(os.getcwd())
    cached = _read_git_cache()
    if cached is not None:
        return cached
    _refresh_git_cache_async()
    branch = _compute_git_branch()
    if branch and branch != "?":
        _write_git_cache(branch)
    return branch if branch else os.path.basename(os.getcwd())


# ── Serial state (rendered verbatim from Rust inventory) ───────────────────

def _serial_text(project_dir: str, session_id: str | None = None) -> str:
    """Render the live per-DUT statusline text from inventory.json.

    Gate: inventory present AND the recorded mcp_pid is alive — a missing
    or dead-server inventory renders NOTHING (no live server => no states).
    `state_text` is the Rust pre-formatted ANSI string, rendered verbatim:
    "unknown"/"stopped" DUTs have empty text and contribute nothing —
    never a made-up healthy state.

    Multi-agent ownership marker: when this session is a GUEST of the
    project (an owner record exists whose live session differs from ours),
    the states are prefixed with a plain `⛨` — the display must never look
    like this agent controls the DUT.
    """
    inv = load_inventory(project_dir)
    if inv is None or not inventory_is_live(inv):
        return ""
    texts = [render_state(dut) for dut in inv.get("duts", [])]
    joined = " ".join(t for t in texts if t)
    if not joined:
        return ""
    owner = load_owner(project_dir)
    if owner is not None and owner_is_alive(owner) and not am_i_owner(owner, session_id):
        return f"⛨ {joined}"
    return joined


def _display_project_dir(payload_cwd: str | None = None) -> str | None:
    """Project discovery for the display path: the hook payload's cwd when
    present, else the process CWD; walk-up + TARGET_CONF only.

    Resolution must never depend on the process CWD alone: the payload cwd
    is the directory the user is actually in. The /dev/shm
    claude-serial-*.lock fallback is deliberately excluded. A session
    whose cwd has no .target.jsonc / inventory must render NO DUT state —
    never another project's (the fallback matches any lock and leaks
    cross-project state, e.g. a foreign project's stale DUT-off).
    """
    if payload_cwd:
        return find_project_dir(allow_lock_fallback=False, start=Path(payload_cwd))
    return find_project_dir(allow_lock_fallback=False)


# ── Main ────────────────────────────────────────────────────────────────────

def main():
    # Parse model + session id + cwd from stdin (Claude Code passes JSON).
    # The payload cwd — not the process CWD — is where the user is; project
    # resolution for the serial display keys on it.
    model = ""
    session_id = None
    payload_cwd = None
    try:
        stdin_data = sys.stdin.read()
        if stdin_data.strip():
            obj = json.loads(stdin_data)
            model = obj.get("model", {}).get("display_name", "")
            session_id = obj.get("session_id")
            payload_cwd = obj.get("cwd") or (obj.get("workspace") or {}).get("current_dir")
    except (json.JSONDecodeError, OSError):
        pass

    # Left side: model + git branch
    left_parts = []
    if model:
        left_parts.append(f"[{model}]")
    left_parts.append(_get_git_branch())
    left = " ".join(left_parts)

    # Right side: serial state from the project's Rust inventory.
    # Skip entirely when CWD is under an excluded path (e.g. allwinner
    # project). The liveness gate inside _serial_text guarantees stale
    # state from a dead server can never render as healthy.
    serial_text = ""
    if not _is_excluded(Path.cwd()):
        project_dir = _display_project_dir(payload_cwd)
        if project_dir:
            serial_text = _serial_text(project_dir, session_id)

    if serial_text:
        print(f"{left}  {serial_text}")
    else:
        print(left)


def _selftest() -> int:
    """Inventory-only regression checks.

    Run with: `uv run hooks/claude/statusline.py --selftest`
    - fixture inventory renders state_text verbatim, joined by spaces;
    - dead pid / missing inventory render nothing (never a phantom state);
    - a subprocess stub raises on ANY exec attempt — the serial hot path
      must never spawn a process.

    Session/guest-view selection (owner vs busy) is covered by
    hooks/tests/test_session_status.py against both renderers.
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

    # The liveness gate identity-checks the recorded pid against /proc
    # (a recycled pid must never make a stale snapshot look live). This
    # selftest process is not sermcp, so stub the identity check for the
    # fixture inventories; the os.kill liveness gate stays fully real.
    import inventory as _inv

    real_identity = _inv._pid_is_mcp_server
    _inv._pid_is_mcp_server = lambda pid: True

    active_text = "\x1b[32m● a:active\x1b[0m"
    crashed_text = "\x1b[31m✗ b:crashed\x1b[0m"

    with tempfile.TemporaryDirectory() as td:
        proj = Path(td) / "proj"
        inv_dir = proj / ".dut-serial"
        inv_dir.mkdir(parents=True)
        duts = [
            {"dut_name": "a", "state": "active", "state_text": active_text},
            {"dut_name": "b", "state": "crashed", "state_text": crashed_text},
            {"dut_name": "c", "state": "unknown", "state_text": ""},
        ]
        inv = {
            "schema_version": 1,
            "written_by": "mcp-server",
            "mcp_pid": os.getpid(),
            "duts": duts,
        }
        inv_path = inv_dir / "inventory.json"

        inv_path.write_text(json.dumps(inv))
        got = _serial_text(str(proj))
        check(
            "live inventory renders state_text verbatim",
            got == f"{active_text} {crashed_text}",
            f"got {got!r}",
        )

        inv_path.write_text(json.dumps({**inv, "mcp_pid": 99999999}))
        check("dead pid renders nothing", _serial_text(str(proj)) == "")

        inv_path.write_text(json.dumps({**inv, "mcp_pid": os.getpid()}))

        # Subprocess stub: any attribute access on the subprocess module is
        # an exec attempt and must fail the test.
        real_subprocess = sys.modules.get("subprocess")

        class _NoExec:
            def __getattr__(self, name):
                raise AssertionError(f"subprocess.{name} called from the serial hot path")

        sys.modules["subprocess"] = _NoExec()
        try:
            got = _serial_text(str(proj))
            check(
                "serial path never execs (subprocess stub intact)",
                got == f"{active_text} {crashed_text}",
            )
        finally:
            if real_subprocess is not None:
                sys.modules["subprocess"] = real_subprocess
            else:
                del sys.modules["subprocess"]

        inv_path.unlink()
        check("missing inventory renders nothing", _serial_text(str(proj)) == "")

    # ── Display path discovery: the statusline must request walk-up-only
    #    project discovery. The /dev/shm lock fallback renders ANOTHER
    #    project's DUT state in a session with no config of its own (the
    #    "wrong DUT-off display" regression).
    real_fpd = globals().get("find_project_dir")
    calls = []

    def _fake_fpd(**kwargs):
        calls.append(kwargs)
        return "/proj" if kwargs.get("allow_lock_fallback") is False else None

    globals()["find_project_dir"] = _fake_fpd
    try:
        got = _display_project_dir()
        check(
            "display path passes allow_lock_fallback=False",
            got == "/proj" and calls == [{"allow_lock_fallback": False}],
            f"got {got!r}, calls {calls!r}",
        )
    finally:
        if real_fpd is not None:
            globals()["find_project_dir"] = real_fpd
        else:
            del globals()["find_project_dir"]

    # ── P1/P2 (ROOT CAUSE A, R1): project resolution must come from the
    #    hook stdin payload, never the process CWD.
    #    P1: payload cwd=A + process CWD=B (both own configs) → renders A.
    #    P2: payload cwd with no config + process CWD inside a foreign
    #        project (+ a foreign lock in /dev/shm) → renders NOTHING.
    import io
    import contextlib

    def _render_with_payload(payload: dict) -> str:
        real_stdin = sys.stdin
        buf = io.StringIO()
        try:
            sys.stdin = io.StringIO(json.dumps(payload))
            with contextlib.redirect_stdout(buf):
                main()
        finally:
            sys.stdin = real_stdin
        return buf.getvalue()

    def _write_inv(proj: Path, dut_name: str, state_text: str) -> None:
        inv_dir = proj / ".dut-serial"
        inv_dir.mkdir(parents=True, exist_ok=True)
        (inv_dir / "inventory.json").write_text(json.dumps({
            "schema_version": 1,
            "written_by": "mcp-server",
            "mcp_pid": os.getpid(),
            "duts": [{
                "dut_name": dut_name,
                "state": "active",
                "state_text": state_text,
            }],
        }))

    old_cwd = os.getcwd()
    old_conf = os.environ.pop("TARGET_CONF", None)
    old_excl = os.environ.pop("TARGET_EXCLUDE", None)
    try:
        with tempfile.TemporaryDirectory() as td:
            base = Path(td)
            proj_a = base / "p1-proj-a"
            proj_b = base / "p1-proj-b"
            proj_a.mkdir()
            proj_b.mkdir()
            (proj_a / ".target.jsonc").write_text("{}")
            (proj_b / ".target.jsonc").write_text("{}")
            text_a = "\x1b[32m● p1-dut-a:active\x1b[0m"
            text_b = "\x1b[31m✗ p1-dut-b:crashed\x1b[0m"
            _write_inv(proj_a, "p1-dut-a", text_a)
            _write_inv(proj_b, "p1-dut-b", text_b)

            os.chdir(proj_b)
            out = _render_with_payload({
                "cwd": str(proj_a),
                "session_id": "p1-session",
                "model": {"display_name": "p1-model"},
                "workspace": {
                    "current_dir": str(proj_a),
                    # project_dir deliberately points at B: resolution must
                    # never use it (version-dependent semantics, D3).
                    "project_dir": str(proj_b),
                },
            })
            check(
                "P1 payload cwd wins: renders project A's DUT state",
                "p1-dut-a" in out,
                f"got {out!r}",
            )
            check(
                "P1 never renders project B's DUT state",
                "p1-dut-b" not in out,
                f"got {out!r}",
            )

            foreign = base / "p2-foreign"
            neutral = base / "p2-neutral"
            foreign.mkdir()
            neutral.mkdir()
            (foreign / ".target.jsonc").write_text("{}")
            _write_inv(foreign, "p2-foreign", "\x1b[31m✗ p2-foreign:crashed\x1b[0m")
            scratch_lock = base / "scratch-lock"
            scratch_lock.write_text(str(foreign))

            import inventory as _inv
            real_glob = _inv.glob.glob
            _inv.glob.glob = lambda pattern: [str(scratch_lock)]
            try:
                os.chdir(foreign)
                out = _render_with_payload({
                    "cwd": str(neutral),
                    "session_id": "p2-session",
                    "model": {"display_name": "p2-model"},
                    "workspace": {"current_dir": str(neutral)},
                })
                # NB: the bare name "p2-foreign" still appears as the
                # git-branch fallback (branch renders from the process
                # CWD by design) — the DUT state text must not.
                check(
                    "P2 payload cwd without config renders NO DUT state (never foreign)",
                    "p2-foreign:crashed" not in out,
                    f"got {out!r}",
                )
            finally:
                _inv.glob.glob = real_glob
    finally:
        os.chdir(old_cwd)
        if old_conf is not None:
            os.environ["TARGET_CONF"] = old_conf
        if old_excl is not None:
            os.environ["TARGET_EXCLUDE"] = old_excl

    # ── R14 (A3/A4): git cache residue must be self-reaping. An
    #    expired-cache read unlinks the file; a dead-writer .lock dir is
    #    reclaimable via the owner-pid file inside it.
    with tempfile.TemporaryDirectory() as td:
        scratch_cwd = Path(td) / "git-cwd"
        scratch_cwd.mkdir()
        prev_cwd = os.getcwd()
        try:
            os.chdir(scratch_cwd)
            cache = _git_cache_path()
            Path(cache).write_text("1000000000.0 stale-branch\n")
            got = _read_git_cache()
            check("R14 expired git cache read returns None", got is None)
            check(
                "R14 expired git cache file is unlinked on read",
                not Path(cache).exists(),
                cache,
            )
            Path(cache).write_text(f"{time.time()} fresh-branch\n")
            check(
                "R14 fresh git cache read returns the branch",
                _read_git_cache() == "fresh-branch",
            )
            Path(cache).unlink()

            reaper = globals().get("_reap_git_cache_lock")
            if reaper is None:
                check(
                    "R14 dead-writer git-cache.lock reaper exists (_reap_git_cache_lock)",
                    False,
                    "not implemented",
                )
            else:
                lock_dir = Path(cache + ".lock")
                os.mkdir(lock_dir)
                (lock_dir / "owner.pid").write_text("99999999")
                os.utime(lock_dir, (time.time() - 100, time.time() - 100))
                check(
                    "R14 dead-writer lock dir is reclaimed regardless of age",
                    bool(reaper(lock_dir)) and not lock_dir.is_dir(),
                )
                os.mkdir(lock_dir)
                (lock_dir / "owner.pid").write_text(str(os.getpid()))
                check(
                    "R14 live-writer lock dir is left alone",
                    not bool(reaper(lock_dir)) and lock_dir.is_dir(),
                )
                if lock_dir.is_dir():
                    (lock_dir / "owner.pid").unlink(missing_ok=True)
                    lock_dir.rmdir()
        finally:
            os.chdir(prev_cwd)

    _inv._pid_is_mcp_server = real_identity
    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(_selftest())
    main()
