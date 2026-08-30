#!/usr/bin/env python3
"""
PreToolUse hook — steer Bash serial interaction toward the Rust MCP.

Dangerous Dev Host operations and `dutabo serial` are DENIED per tool
call via a PreToolUse permission decision (`continue: true` +
`permissionDecision: "deny"`): only that one tool call is blocked and
the reason is injected into Claude's context, so the agent can switch
to MCP tools within the same turn. `continue: false` is deliberately
NOT used — it stops the entire Claude turn after the hook runs instead
of blocking the single call. Raw serial/relay access is not denied; it
gets a "please prefer MCP tools" hint (`continue: true` + systemMessage).

Data source: dev-host IPs come from the Rust-written
`.dut-serial/inventory.json` (dev_hosts[].ip, multi-host). Python parses
ZERO config. FAIL-CLOSED (the A4 lesson): when the inventory is
absent or its recorded MCP pid is dead, dev-host-shaped dangerous
commands (ssh/scp/rsync/sftp with a destructive verb, or raw relay
protocol bytes) to ANY non-loopback IPv4 address are DENIED with reason
"inventory unavailable" — a missing inventory must never silently allow
a dangerous command through.
"""

import json
import re
import sys
from pathlib import Path

_HOOK_DIR = Path(__file__).resolve().parent
if str(_HOOK_DIR) not in sys.path:
    sys.path.insert(0, str(_HOOK_DIR))

from inventory import find_project_root, load_inventory, inventory_is_live


# ── Inventory access ──────────────────────────────────────────────────────

def _dev_host_ips(inv: dict) -> list:
    """Known dev-host IPs from inventory.dev_hosts (multi-host safe)."""
    if not isinstance(inv, dict):
        return []
    return [str(h["ip"]) for h in inv.get("dev_hosts", []) if h.get("ip")]


def _load_live_inventory(project_dir: str):
    """Inventory + liveness in one call; None = unavailable (fail-closed).

    Liveness is the recorded mcp_pid via os.kill — the server writes
    inventory at startup, so written_at can be old while the server is
    healthy. A dutabo-written bootstrap (mcp_pid None) is NOT live: the
    session-start cache enumerates names but proves no server exists.
    """
    inv = load_inventory(project_dir)
    if inv is None or not inventory_is_live(inv):
        return None
    return inv


# ── Dangerous Dev Host command detection ──────────────────────────────────

# Destructive verbs that must never run on a Dev Host.
_DANGEROUS = [
    (
        r"(?:reboot|shutdown\s+now|poweroff)",
        "reboot/shutdown dev host",
    ),
    (
        r"(?:dd\s+if=|dd\s+of=/dev)",
        "dd disk write",
    ),
    (
        r"(?:mkfs\.|mke2fs|mkfs)",
        "mkfs format",
    ),
    (
        r"rm\s+-rf\s+/",
        "rm -rf /",
    ),
    (
        r"(?:apt\s+install|apt\s+remove|dpkg)",
        "apt/pkg modify",
    ),
]

_IPV4 = re.compile(r"((?:\d{1,3}\.){3}\d{1,3})")
_SSH_FAMILY = re.compile(r"\b(?:ssh|scp|rsync|sftp)\b")
# Raw relay protocol writes: literal \x escapes in shell strings plus the
# relay 4-byte header (\xa0\x01...). In the command string '\xa0' appears
# as backslash-x-a-0 (4 chars); the regex \\x matches the literal bytes.
_RAW_RELAY = re.compile(r"\\xa0\\x01|printf\s+.*\\x[0-9a-fA-F]")


def _non_loopback_ip(command: str) -> str:
    """First IPv4 literal in the command that is not loopback."""
    for m in _IPV4.finditer(command):
        ip = m.group(1)
        if not ip.startswith("127."):
            return ip
    return ""


def _dangerous_label(command: str) -> str:
    """Label of the first destructive verb present, or ""."""
    for pattern, label in _DANGEROUS:
        if re.search(pattern, command):
            return label
    return ""


def _known_host_reason(command: str, ip: str, label: str) -> str:
    if label == "raw relay control":
        return (
            f"[CRITICAL] Dangerous operation ({label}) targeting Dev Host ({ip}) DENIED. "
            "Relay ownership belongs to the project MCP. Use serial_reset or another "
            "appropriate MCP power-control tool."
        )
    return (
        f"[CRITICAL] Dangerous operation ({label}) targeting Dev Host ({ip}) DENIED. "
        "The Dev Host is a PRODUCTION machine — NEVER modify it. "
        "To flash a target board: the image must be UPLOADED to the Dev Host, "
        "then upgrade_tool flashes the TARGET via USB OTG. "
        "Use dutabo uf/dutabo flash-kernel, or rk-build skill scripts."
    )


def _unavailable_reason(command: str, ip: str, label: str) -> str:
    if label == "raw relay control":
        return (
            f"[CRITICAL] Dangerous operation ({label}) targeting dev host ({ip}) DENIED — "
            "inventory unavailable (no live MCP server wrote .dut-serial/inventory.json). "
            "Relay ownership belongs to the project MCP. Use serial_reset or another "
            "appropriate MCP power-control tool."
        )
    return (
        f"[CRITICAL] Dangerous operation ({label}) targeting dev host ({ip}) DENIED — "
        "inventory unavailable (no live MCP server wrote .dut-serial/inventory.json, "
        "so dev-host IPs cannot be verified — refusing any dev-host-shaped command). "
        "The Dev Host is a PRODUCTION machine — NEVER modify it. "
        "To flash a target board: the image must be UPLOADED to the Dev Host, "
        "then upgrade_tool flashes the TARGET via USB OTG. "
        "Use dutabo uf/dutabo flash-kernel, or rk-build skill scripts."
    )


def _dangerous_deny_reason(command: str, known_ips, inventory_available: bool):
    """Deny reason for a dangerous dev-host command, or None.

    inventory_available + known_ips: deny only commands targeting a KNOWN
    dev host (other machines stay untouched).
    inventory unavailable (fail-closed, A4): deny ssh-family destructive
    commands and raw relay protocol writes to ANY non-loopback IPv4
    address — never a silent allow.
    """
    if inventory_available:
        for ip in [i for i in known_ips if i]:
            if not re.search(re.escape(ip), command):
                continue
            if _RAW_RELAY.search(command):
                return _known_host_reason(command, ip, "raw relay control")
            label = _dangerous_label(command)
            if label:
                return _known_host_reason(command, ip, label)
        return None

    # Fail-closed: no live inventory to verify dev hosts against.
    if _SSH_FAMILY.search(command):
        ip = _non_loopback_ip(command)
        label = _dangerous_label(command)
        if ip and label:
            return _unavailable_reason(command, ip, label)
    if _RAW_RELAY.search(command):
        ip = _non_loopback_ip(command)
        if ip:
            return _unavailable_reason(command, ip, "raw relay control")
    return None


# Patterns that indicate raw serial access — should use MCP instead
def _build_serial_patterns(host_pattern: str) -> list:
    """Serial raw-access hint patterns; `host_pattern` is an escaped
    alternation of the known dev-host IPs, or a generic 192.168.x fallback
    when the inventory is unavailable (hints are non-blocking)."""
    return [
        rf"nc\s+.*{host_pattern}",       # netcat to dev host
        rf"ncat\s+.*{host_pattern}",
        r"tio\s+/dev/tty",               # tio serial terminal
        r"screen\s+/dev/tty",            # screen serial
        r"cu\s+-l\s+/dev/tty",           # cu serial
        r"stty\s+-F\s+/dev/tty",         # stty config
        r"picocom\s+/dev/tty",
        r"minicom\s+/dev/tty",
        r">\s*/dev/tty",                 # direct write to serial
        r"echo.*>\s*/dev/tty",
        r"socat\s+.*tty",                # socat to serial
        # Relay raw byte patterns: match literal \x prefix in shell strings.
        # In the command string, '\xa0' appears as backslash-x-a-0 (4 chars).
        # The regex \\x matches a literal backslash followed by 'x'.
        r"printf\s+.*\\x[0-9a-fA-F].*2000",  # relay raw packet to port 2000
        r"\\xa0\\x01",                       # relay protocol header bytes
    ]


# Patterns that are valid but should prefer MCP (built dynamically with dev host IPs)
def _build_relay_patterns(host_pattern: str) -> list:
    """Relay raw-access hint patterns; same host_pattern rule as serial."""
    return [
        rf"nc\s+.*{host_pattern}\s+2001",  # relay port
        r">\s*/dev/tcp/.*2001",
    ]


def _deny(reason: str) -> None:
    """Block a single tool call with a PreToolUse permission decision.

    `continue: true` + `permissionDecision: "deny"` blocks only this one
    tool call and injects `reason` into Claude's context — the turn keeps
    going and the agent can immediately switch to the MCP tools named in
    the reason. `continue: false` must NOT be used here: it halts the
    entire Claude turn after the hook runs, so the agent never sees the
    guidance and retries the same command after the user resumes.
    """
    print(json.dumps({
        "continue": True,
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        },
    }))


# ── DENY: spawning the MCP server binary directly ──────────────────────
# A background `sermcp`/`sermcp --http` started from the Bash tool has its
# stdin tied to the (never-closing) shell pipe: it holds the project lock
# and the owner slot indefinitely, and every later connection of this same
# agent bridges in as a read-only guest of a server nobody remembers
# starting. The lifecycle belongs to the agent host (.mcp.json stdio
# spawn) and the session hooks — never to a stray background process.
# `dutabo` stays allowed (it talks to the HTTP endpoint).
#
# Wrapper parsing is a guard against accidental launches, not a shell sandbox.
# Runtime endpoint locks remain authoritative when commands are constructed dynamically.

_ENV_ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
# Wrapper options that consume the NEXT token as their argument.
_ARG_OPTIONS = {
    "env": {"-u", "--unset", "-C", "--chdir"},
    "sudo": {"-u", "-g", "-p", "-C", "-D", "-h", "-r", "-t"},
    "timeout": {"-k", "-s", "--signal", "--kill-after"},
    "nice": {"-n", "--adjustment"},
    "stdbuf": {"-o", "-e", "-i"},
}
_PLAIN_WRAPPERS = {"nohup", "setsid", "exec", "command"}
_ARG_WRAPPERS = {"sudo", "env", "timeout", "nice", "stdbuf"}
# Read-only invocations that never connect, lock, or serve.
_INSPECT_ONLY_FLAGS = {"--help", "-h", "--version", "-V", "--dry-run"}


def _strip_wrappers(tokens: list) -> list:
    """Drop leading wrapper tokens (`nohup`, `env VAR=...`, `sudo -u x`,
    `timeout 30`, `nice -n 5`, bare `VAR=v` prefixes, ...) so the first
    remaining token is the real command."""
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        if _ENV_ASSIGN.match(tok):
            i += 1
            continue
        wrapper = Path(tok).name
        if wrapper in _PLAIN_WRAPPERS:
            i += 1
            while i < len(tokens) and tokens[i].startswith("-"):
                if wrapper == "command" and tokens[i] in {"-v", "-V"}:
                    return []
                if wrapper == "exec" and tokens[i] == "-a":
                    i += 1
                i += 1
            continue
        if wrapper in _ARG_WRAPPERS:
            i += 1
            # Skip the wrapper's own flags (and their separate arguments).
            while i < len(tokens) and tokens[i].startswith("-"):
                if tokens[i] in _ARG_OPTIONS[wrapper] and i + 1 < len(tokens):
                    i += 1
                i += 1
            # `timeout <dur>` / `nice <adj>` take one bare numeric token.
            if wrapper in ("timeout", "nice") and i < len(tokens) and re.match(
                r"^\+?-?\d", tokens[i]
            ):
                i += 1
            continue
        break
    return tokens[i:]


def _first_sermcp_argv(command: str, depth: int = 0):
    """Return the first unsafe launch, checking every shell command position.

    Quote-aware tokenization avoids treating quoted prose as shell syntax.
    Shell wrappers are inspected recursively; arbitrary shell expansion is
    deliberately outside this best-effort hook's guarantees.
    """
    import shlex

    if depth > 8:
        return None
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()\n")
        lexer.whitespace = " \t\r"
        tokens = list(lexer)
    except ValueError:
        return None
    segments, current = [], []
    for token in tokens:
        if token and all(c in ";&|()\n" for c in token):
            segments.append(current)
            current = []
        else:
            current.append(token)
    segments.append(current)
    for segment in segments:
        rest = _strip_wrappers(segment)
        if not rest:
            continue
        binary = Path(rest[0]).name
        if binary in {"sh", "bash", "dash", "zsh"}:
            for i, arg in enumerate(rest[1:], 1):
                if arg.startswith("-") and "c" in arg and i + 1 < len(rest):
                    nested = _first_sermcp_argv(rest[i + 1], depth + 1)
                    if nested:
                        return nested
                    break
        if binary == "cargo" and len(rest) > 1 and rest[1] == "run":
            # Cargo's help does not execute a binary; explicit dutabo is a client.
            if "--help" in rest[2:] and "--" not in rest:
                continue
            if "--bin" not in rest or rest[rest.index("--bin") + 1:][:1] == ["sermcp"]:
                args = rest[rest.index("--") + 1:] if "--" in rest else []
                if not any(flag in _INSPECT_ONLY_FLAGS for flag in args):
                    return ["sermcp", *args]
        if binary == "sermcp" and not any(flag in _INSPECT_ONLY_FLAGS for flag in rest[1:]):
            return rest
    return None


def _sermcp_spawn_reason(command: str, project_dir: str = ""):
    argv = _first_sermcp_argv(command)
    if argv is None:
        return None
    if any(flag in _INSPECT_ONLY_FLAGS for flag in argv[1:]):
        return None

    reason = (
        "[BLOCKED] Do not start the sermcp MCP server yourself — a "
        "background server holds the project lock and the owner slot "
        "indefinitely and breaks every later MCP connection of this "
        "session. The server is managed automatically: use the MCP "
        "tools (serial_get_state, serial_exec, ...), or `dutabo` for "
        "CLI access. To restart a wedged server run: "
        "kill $(cat .dut-serial/mcp.pid)"
    )

    # Detached HTTP must retain an exit mechanism owned by the session.
    args = argv[1:]
    for idx, flag in enumerate(args):
        if flag == "--idle-timeout=0" or (
            flag == "--idle-timeout"
            and idx + 1 < len(args)
            and args[idx + 1].strip() == "0"
        ):
            reason += (
                " --idle-timeout 0 is FORBIDDEN: it disables the idle "
                "self-exit; hosts without SessionEnd need verified "
                "session-process supervision to reclaim the endpoint. If a "
                "longer-lived server is truly needed, ask the USER to "
                "start it manually."
            )
            break

    # A foreign TARGET_CONF can contend for another project's hardware.
    if project_dir:
        proj = Path(project_dir).resolve()
        for token in argv[1:] + re.findall(r"TARGET_CONF=\S+", command):
            value = token.split("=", 1)[1] if token.startswith("TARGET_CONF=") else ""
            if not value:
                continue
            conf = Path(value).resolve()
            try:
                conf.relative_to(proj)
            except ValueError:
                reason += (
                    f" TARGET_CONF={value} belongs to ANOTHER project: "
                    "an engine started here will fight that project's "
                    "engine for the same physical serial port and violate "
                    "project exclusivity. Start it in "
                    "its own project session, or ask the USER to do it."
                )
                break

    return reason


def main():
    try:
        hook_input = json.loads(sys.stdin.read())
    except (json.JSONDecodeError, OSError):
        print(json.dumps({"continue": True}))
        sys.exit(0)

    tool_name = hook_input.get("tool_name", "")
    tool_input = hook_input.get("tool_input", {})

    # Only intercept Bash commands
    if tool_name != "Bash":
        print(json.dumps({"continue": True}))
        sys.exit(0)

    command = tool_input.get("command", "")
    if not command:
        print(json.dumps({"continue": True}))
        sys.exit(0)

    cwd = hook_input.get("cwd")
    project_dir = find_project_root(allow_lock_fallback=False, start=Path(cwd) if isinstance(cwd, str) and cwd else None)
    if not project_dir:
        print(json.dumps({"continue": True}))
        sys.exit(0)

    inv = _load_live_inventory(project_dir)
    known_ips = _dev_host_ips(inv) if inv is not None else []

    # ── CRITICAL: Deny dangerous commands targeting the Dev Host ──
    # The Dev Host is a PRODUCTION machine (the SDK build / flash host). It
    # bridges serial and runs upgrade_tool to flash the TARGET board via
    # USB OTG.
    # NEVER reboot, flash, dd, rm -rf, or modify the Dev Host itself.
    # Denied per tool call (not continue: false) so Claude keeps working
    # and can re-route through the MCP tools named in the reason.
    reason = _dangerous_deny_reason(command, known_ips, inv is not None)
    if reason:
        _deny(reason)
        sys.exit(0)

    # ── DENY: dutabo serial — interactive serial console, human-only ──
    # The Agent must use MCP tools (serial_send_command, serial_get_state, etc.)
    # instead of taking over the serial port interactively. Denied per tool
    # call so Claude can switch to MCP tools within the same turn.
    if re.search(r"dutabo\s+serial", command):
        _deny(
            "[BLOCKED] dutabo serial is restricted to human use only. "
            "The Agent must use MCP tools: serial_send_command, serial_get_state, "
            "serial_reset, serial_enter_uboot, serial_uboot_command. "
            "dutabo serial is an interactive console session that takes over "
            "the serial port (pauses the MCP engine) and is not suitable for "
            "automated Agent interaction."
        )
        sys.exit(0)

    # ── DENY: spawning the MCP server binary directly ──
    # See _sermcp_spawn_reason for the rationale.
    reason = _sermcp_spawn_reason(command, project_dir)
    if reason:
        _deny(reason)
        sys.exit(0)

    host_pattern = (
        "|".join(re.escape(ip) for ip in known_ips)
        if known_ips
        else r"192\.168\.\d+\.\d+"
    )
    serial_patterns = _build_serial_patterns(host_pattern)
    relay_patterns = _build_relay_patterns(host_pattern)

    # Check for raw serial access — DENY (multi-agent isolation): raw nc/tio/
    # screen on the dev-host serial port fights the owning MCP engine for the
    # endpoint. The agent must use MCP tools, which handle locking/ownership.
    for pattern in serial_patterns:
        if re.search(pattern, command):
            _deny(
                "[BLOCKED] Raw serial access is not allowed — it conflicts "
                "with the MCP engine that owns the serial endpoint. "
                "Use MCP tools instead: serial_send_command, serial_get_state, "
                "serial_reset, serial_enter_uboot, serial_get_logs. "
                "Interactive access is `dutabo serial` (human terminal only)."
            )
            sys.exit(0)

    # Check for relay direct access — DENY: the relay is physical power
    # control and must go through MCP (which locks it per multi-agent rules).
    for pattern in relay_patterns:
        if re.search(pattern, command):
            _deny(
                "[BLOCKED] Relay control via raw TCP is not allowed. "
                "Use serial_reset or serial_enter_uboot instead — "
                "MCP handles the 4-byte relay protocol and endpoint locking."
            )
            sys.exit(0)

    # Check for Python scripts that bypass MCP — warn (don't block)
    if "burnin" in command.lower() and "python" in command.lower():
        print(json.dumps({
            "continue": True,
            "systemMessage": (
                "[MCP-ENFORCE] External burn-in script detected. "
                "Consider using MCP tools for burn-in testing: "
                "serial_enter_uboot + serial_send_command('boot') in a loop. "
                "MCP provides logging, state tracking, and relay control."
            )
        }))
        sys.exit(0)

    print(json.dumps({"continue": True}))


def _selftest() -> int:
    """Regression checks for the inventory-based dev-host protection (A4).

    Run with: `uv run hooks/claude/pre-tool-use.py --selftest`
    Covers: known-host denies (live inventory), fail-closed denies
    (inventory unavailable -> ANY non-loopback target), non-dangerous and
    loopback pass-through, and the relay-hint boundary (a plain
    /dev/tcp/2001 echo stays a hint, never a deny).
    """
    failed = 0

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal failed
        if cond:
            print(f"OK    {name}")
        else:
            print(f"FAIL  {name}{(' — ' + detail) if detail else ''}")
            failed += 1

    DEV = "10.1.2.3"
    OTHER = "192.168.9.9"

    # ── Live inventory: deny only commands targeting a KNOWN dev host.
    ips = [DEV]
    r = _dangerous_deny_reason(f'ssh root@{DEV} "reboot"', ips, True)
    check(
        "known-host dangerous ssh is denied (PRODUCTION wording)",
        r is not None and "PRODUCTION" in r and "dutabo uf" in r,
        f"got {r!r}",
    )
    check(
        "known-host deny names the specific host",
        r is not None and DEV in r,
        f"got {r!r}",
    )
    check(
        "unrelated host is NOT denied with live inventory",
        _dangerous_deny_reason(f'ssh root@{OTHER} "reboot"', ips, True) is None,
    )
    check(
        "loopback is never denied",
        _dangerous_deny_reason('ssh root@127.0.0.1 "reboot"', ips, True) is None,
    )

    # ── Fail-closed: no inventory -> any non-loopback dev-host-shaped
    #    dangerous command is denied with the A4 reason.
    r = _dangerous_deny_reason(f'ssh root@{OTHER} "reboot"', [], False)
    check(
        "fail-closed denies ssh to any non-loopback IP",
        r is not None and "inventory unavailable" in r,
        f"got {r!r}",
    )
    check(
        "fail-closed reason still names MCP tools + PRODUCTION",
        r is not None and "dutabo uf" in r and "PRODUCTION" in r,
        f"got {r!r}",
    )
    check(
        "fail-closed scp with destructive verb is denied",
        _dangerous_deny_reason(f'scp file root@{OTHER}:/dev/sda && dd if=/dev/zero', [], False) is not None,
    )
    check(
        "fail-closed raw relay bytes to non-loopback are denied",
        _dangerous_deny_reason(f"printf '\\xa0\\x01' > /dev/tcp/{OTHER}/2000", [], False) is not None,
    )
    check(
        "plain /dev/tcp/2001 echo stays a hint (never a deny)",
        _dangerous_deny_reason(f"echo hi > /dev/tcp/{OTHER}/2001", [], False) is None,
    )
    check(
        "non-dangerous ssh is not denied even without inventory",
        _dangerous_deny_reason(f"ssh ubuntu@{OTHER} ls /", [], False) is None,
    )
    check(
        "harmless command is not denied without inventory",
        _dangerous_deny_reason("echo hello", [], False) is None,
    )

    # ── Inventory extraction.
    inv = {
        "schema_version": 1,
        "dev_hosts": [
            {"ip": DEV, "user": "u"},
            {"ip": OTHER, "user": "u"},
            {"ip": "", "user": "u"},
        ],
    }
    check(
        "_dev_host_ips extracts every host ip (multi-host)",
        _dev_host_ips(inv) == [DEV, OTHER],
    )
    check(
        "_dev_host_ips is empty for a non-dict",
        _dev_host_ips("garbage") == [],
    )

    # ── sermcp spawn block: the agent must never background the server
    #    binary itself (a stray server pins the project lock and the owner
    #    slot forever). Wrapper-hidden spawns (nohup/env/setsid/timeout/
    #    stdbuf/VAR=v) are blocked too.
    #    Mentions and inspect-only uses stay allowed.
    check(
        "bare sermcp spawn is blocked",
        _sermcp_spawn_reason("sermcp --http 127.0.0.1:3005 &") is not None,
    )
    check(
        "backgrounded sermcp after a separator is blocked",
        _sermcp_spawn_reason("sleep 1 && sermcp") is not None,
    )
    check(
        "absolute-path spawn is blocked",
        _sermcp_spawn_reason("/home/u/.cargo/bin/sermcp --http") is not None,
    )
    check(
        "sudo sermcp is blocked",
        _sermcp_spawn_reason("sudo sermcp --http") is not None,
    )
    check(
        "sudo -u <user> sermcp is blocked",
        _sermcp_spawn_reason("sudo -u loh sermcp --http") is not None,
    )
    check(
        "nohup sermcp is blocked",
        _sermcp_spawn_reason(
            "nohup sermcp --http 127.0.0.1:3083 > /tmp/s.out 2>&1 &"
        ) is not None,
    )
    check(
        "VAR=v sermcp env-prefix spawn is blocked",
        _sermcp_spawn_reason("RUST_LOG=debug sermcp --http 127.0.0.1:3001 &") is not None,
    )
    check(
        "setsid sermcp is blocked",
        _sermcp_spawn_reason("setsid sermcp --http 127.0.0.1:3001") is not None,
    )
    check(
        "timeout-wrapped sermcp is blocked",
        _sermcp_spawn_reason("timeout 30 sermcp --http 127.0.0.1:3001") is not None,
    )
    check(
        "stdbuf-wrapped sermcp is blocked",
        _sermcp_spawn_reason("stdbuf -oL sermcp --http 127.0.0.1:3001") is not None,
    )
    check(
        "sermcp inside $(...) is blocked",
        _sermcp_spawn_reason("echo started $(nohup sermcp &)") is not None,
    )
    check(
        "cross-project detached launch is blocked (nohup env TARGET_CONF)",
        _sermcp_spawn_reason(
            "cd /proj/a && nohup env TARGET_CONF=/proj/b/.target.jsonc "
            "sermcp --http 127.0.0.1:3083 --idle-timeout 0 "
            "> /tmp/s.out 2>&1 & sleep 6; curl -s http://127.0.0.1:3083/health"
        ) is not None,
    )

    # Graded reasons on the incident command.
    incident = (
        "nohup env TARGET_CONF=/proj/b/.target.jsonc sermcp --http "
        "127.0.0.1:3083 --idle-timeout 0"
    )
    r = _sermcp_spawn_reason(incident, "/proj/a")
    check(
        "incident deny names the idle-timeout ban",
        r is not None and "--idle-timeout 0 is FORBIDDEN" in r,
        f"got {r!r}",
    )
    check(
        "incident deny names the cross-project hazard",
        r is not None and "ANOTHER project" in r,
        f"got {r!r}",
    )
    r = _sermcp_spawn_reason(
        "sermcp --http 127.0.0.1:3005 --idle-timeout=0", "/proj/a"
    )
    check(
        "--idle-timeout=0 (equals form) also triggers the ban",
        r is not None and "--idle-timeout 0 is FORBIDDEN" in r,
        f"got {r!r}",
    )
    r = _sermcp_spawn_reason(
        "nohup env TARGET_CONF=/proj/a/.target.jsonc sermcp --http :3005",
        "/proj/a",
    )
    check(
        "same-project TARGET_CONF blocks without cross-project wording",
        r is not None and "ANOTHER project" not in r,
        f"got {r!r}",
    )

    check(
        "dutabo stays allowed",
        _sermcp_spawn_reason("dutabo state --dut rk3576") is None,
    )
    check(
        "kill-by-pidfile restart guidance stays allowed",
        _sermcp_spawn_reason("kill $(cat .dut-serial/mcp.pid)") is None,
    )
    check(
        "prose mentioning sermcp is not blocked",
        _sermcp_spawn_reason("grep sermcp .mcp.json && cat .dut-serial/mcp.log") is None,
    )
    check(
        "inspect-only flags stay allowed (--help)",
        _sermcp_spawn_reason("sermcp --help") is None,
    )
    check(
        "inspect-only flags stay allowed (--version, absolute path)",
        _sermcp_spawn_reason("/home/u/.cargo/bin/sermcp --version") is None,
    )
    check(
        "inspect-only flags stay allowed (--dry-run behind env)",
        _sermcp_spawn_reason("env RUST_LOG=info sermcp --dry-run") is None,
    )
    check(
        "cargo run server launch is denied",
        _sermcp_spawn_reason("cargo run --release -- --http 127.0.0.1:3000") is not None,
    )
    check(
        "quoted prose is not blocked",
        _sermcp_spawn_reason("echo 'nohup sermcp --http'") is None,
    )

    for command in (
        'sermcp --help; nohup sermcp --http',
        '"/home/u/bin/sermcp" --http',
        '/usr/bin/nohup /usr/bin/env A=b sermcp --http',
        "bash -lc 'nohup sermcp --http &'",
        'sudo -n sermcp --http',
    ):
        check(f"wrapper/quoted/multiple launch denied: {command}", _sermcp_spawn_reason(command) is not None)
    check("quoted separators remain prose", _sermcp_spawn_reason("echo 'example; sermcp --http'") is None)

    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(_selftest())
    main()
