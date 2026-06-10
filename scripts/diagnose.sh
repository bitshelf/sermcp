#!/bin/bash
# Serial MCP Diagnostic — run when MCP is offline or misbehaving
# Usage: bash scripts/diagnose.sh

# Dev host IP: $DEHOST env override, else the first dev_hosts[].ip from
# .target.jsonc (the project config). Empty → the SSH sections skip.
dev_host_ip() {
    python3 - <<'PY'
import json, pathlib, re

def strip_comments(text):
    out = []
    i, n = 0, len(text)
    in_str = False
    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if in_str:
            out.append(c)
            if c == "\\":
                out.append(nxt)
                i += 2
                continue
            if c == '"':
                in_str = False
            i += 1
            continue
        if c == '"':
            in_str = True
            out.append(c)
            i += 1
            continue
        if c == "/" and nxt == "/":
            while i < n and text[i] != "\n":
                i += 1
            continue
        if c == "/" and nxt == "*":
            i += 2
            while i + 1 < n and not (text[i] == "*" and text[i + 1] == "/"):
                i += 1
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)

p = pathlib.Path(".target.jsonc")
if p.exists():
    try:
        text = strip_comments(p.read_text())
        text = re.sub(r",\s*([}\]])", r"\1", text)  # trailing commas
        hosts = json.loads(text).get("dev_hosts") or []
        ip = hosts[0].get("ip") if hosts else None
        if ip:
            print(ip)
    except Exception:
        pass
PY
}

DEHOST="${DEHOST:-$(dev_host_ip)}"
RED='\033[31m'; GREEN='\033[32m'; YELLOW='\033[33m'; NC='\033[0m'
ok() { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $1"; }
fail() { echo -e "  ${RED}✗${NC} $1"; }

echo "=== Serial MCP Diagnostic ==="
echo "Dev Host: $DEHOST"
echo ""

# 1. Binary check
echo "[1] MCP Binary"
if command -v sermcp &>/dev/null; then
    ok "$(sermcp --version 2>&1 | head -1)"
else
    fail "sermcp not in PATH — run: deploy-all.sh"
fi

# 2. Dev host reachable
echo "[2] Dev Host SSH"
if [ -z "$DEHOST" ]; then
    warn "no dev host IP — set DEHOST or add dev_hosts[].ip to .target.jsonc"
else
    if ssh -o ConnectTimeout=3 "$DEHOST" "echo ok" &>/dev/null; then
        ok "$DEHOST reachable"
    else
        fail "$DEHOST unreachable"
    fi
fi

# 3. ser2net ports
echo "[3] ser2net ports"
if [ -n "$DEHOST" ]; then
    for port in 2000 2001 2002 2008; do
        if ssh "$DEHOST" "ss -tlnp | grep -q :$port" 2>/dev/null; then
            ok "port $port LISTEN"
        else
            warn "port $port not listening"
        fi
    done
fi

# 4. USB devices
echo "[4] USB serial devices"
if [ -n "$DEHOST" ]; then
    # ls -l puts the device path LAST; match on the extracted field (the
    # old case matched the whole line against /dev/tty* and never fired).
    ssh "$DEHOST" "ls -la /dev/ttyACM* /dev/ttyUSB* 2>/dev/null" | awk '{print $NF}' | while read -r line; do
        case "$line" in
            /dev/tty*) ok "$line" ;;
            *) ok "  $line" ;;
        esac
    done
    if ! ssh "$DEHOST" "ls /dev/ttyACM* /dev/ttyUSB* 2>/dev/null | grep -q ."; then
        warn "no serial ttys on dev host — check USB cables"
    fi
fi

# 5. MCP processes
echo "[5] MCP processes"
MCP_COUNT=$(ps aux | grep -c '[d]ebug-console-mcp')
if [ "$MCP_COUNT" -eq 1 ]; then
    ok "1 MCP running"
elif [ "$MCP_COUNT" -gt 1 ]; then
    warn "$MCP_COUNT MCP processes (may conflict) — run: pkill sermcp"
else
    warn "no MCP running — Claude Code will auto-start on next tool call"
fi

# 6. Lock files
echo "[6] Lock files"
for lock in /dev/shm/claude-serial-*.lock; do
    if [ -f "$lock" ]; then
        ok "$(basename $lock) → $(cat $lock)"
    fi
done
if ! ls /dev/shm/claude-serial-*.lock &>/dev/null; then
    warn "no lock files — session-start may not have run yet"
fi

# 7. .dut-serial state — scan every project under $RK_PROJ_DIR (the SDK
# projects mount root; e.g. `RK_PROJ_DIR=/path/to/projects`).
# Unset → section skipped.
echo "[7] Project state"
if [ -n "${RK_PROJ_DIR:-}" ]; then
    for dir in "$RK_PROJ_DIR"/*/.dut-serial; do
        if [ -d "$dir" ]; then
            proj="$(dirname "$dir")"
            for statefile in "$dir"/*/target-state; do
                if [ -f "$statefile" ]; then
                    alias="$(basename "$(dirname "$statefile")")"
                    state="$(cat "$statefile")"
                    ok "$(basename "$proj")/$alias: $state"
                fi
            done
        fi
    done
fi

echo ""
echo "=== Diagnostic Complete ==="
