#!/usr/bin/env python3
"""Expose sermcp DUT state to Codex hooks and terminals.

Reads the Rust-written `.dut-serial/inventory.json` (schema_version 1) and
renders each DUT's pre-formatted ANSI `state_text` verbatim — the
state -> color mapping lives only in state_manager.rs and cannot drift.
Project discovery is the shared walk-up (inventory.json -> .target.jsonc).
No subprocess, no network: file read + os.kill per invocation.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

_HOOK_DIR = Path(__file__).resolve().parent
_CLAUDE_DIR = _HOOK_DIR.parent / "claude"
if str(_CLAUDE_DIR) not in sys.path:
    sys.path.insert(0, str(_CLAUDE_DIR))

from inventory import find_project_root, load_inventory, inventory_is_live


def project_root(start: Path) -> Path | None:
    """Resolve the project root from `start` via the shared walk-up
    (TARGET_CONF env -> .dut-serial/inventory.json -> .target.jsonc ->
    /dev/shm lock fallback)."""
    previous = os.getcwd()
    try:
        os.chdir(start)
        root = find_project_root()
        return Path(root) if root else None
    finally:
        os.chdir(previous)


def inventory_states(root: Path) -> tuple[dict | None, list[str], list[str]]:
    """(inventory, ansi texts, plain texts) with the liveness gate applied:
    a dead server contributes NO states — explicit degradation ("MCP not
    running"), never a stale render. ANSI texts are Rust's pre-formatted
    state_text (verbatim); plain texts are state_plain for non-TTY output.
    DUTs with empty text (stopped/unknown) contribute nothing — a
    made-up healthy state is never rendered."""
    inv = load_inventory(str(root))
    if inv is None or not inventory_is_live(inv):
        return inv, [], []
    with_text = [d for d in inv.get("duts", []) if d.get("state_text")]
    return (
        inv,
        [d["state_text"] for d in with_text],
        [d["state_plain"] for d in with_text],
    )


def plain_status(inv: dict | None, texts: list[str]) -> str:
    if texts:
        return "DUT serial: " + " ".join(texts)
    if inv is None or not inventory_is_live(inv):
        return "DUT serial: MCP not running"
    return "DUT serial: no live state"


def colored_status(inv: dict | None, texts: list[str]) -> str:
    # texts carry Rust's ANSI already — render verbatim, never re-map.
    if texts:
        return " ".join(texts)
    if inv is None or not inventory_is_live(inv):
        return "\033[90m○ DUT serial: MCP not running\033[0m"
    return "\033[90m○ DUT serial: no live state\033[0m"


def hook_output(event: str, inv: dict | None, texts: list[str]) -> dict[str, object]:
    status = plain_status(inv, texts)
    result: dict[str, object] = {
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": status,
        }
    }
    # Critical alerts only from a LIVE server — a dead server's crash flag
    # is stale and must not pester the session (user-prompt-submit owns
    # the dead-server messaging).
    critical = (
        inv is not None
        and inventory_is_live(inv)
        and any(d.get("critical") for d in inv.get("duts", []))
    )
    if critical:
        result["systemMessage"] = status
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--hook", action="store_true", help="emit Codex hook JSON")
    parser.add_argument("--watch", action="store_true", help="refresh continuously")
    parser.add_argument("--interval", type=float, default=0.5)
    parser.add_argument("--root", type=Path)
    args = parser.parse_args()

    payload: dict[str, object] = {}
    if args.hook:
        try:
            payload = json.load(sys.stdin)
        except (json.JSONDecodeError, OSError):
            payload = {}

    start = args.root or Path(str(payload.get("cwd") or os.getcwd()))
    root = project_root(start.resolve())
    if root:
        inv, ansi_texts, plain_texts = inventory_states(root)
    else:
        inv, ansi_texts, plain_texts = None, [], []

    if args.hook:
        event = str(payload.get("hook_event_name") or "SessionStart")
        print(json.dumps(hook_output(event, inv, plain_texts), ensure_ascii=False))
        return 0

    if not args.watch:
        print(colored_status(inv, ansi_texts) if sys.stdout.isatty() else plain_status(inv, plain_texts))
        return 0

    try:
        previous = ""
        while True:
            if root:
                inv, ansi_texts, _plain = inventory_states(root)
            current = colored_status(inv, ansi_texts)
            if current != previous:
                sys.stdout.write(f"\r\033[2K{current}")
                sys.stdout.flush()
                previous = current
            time.sleep(max(args.interval, 0.1))
    except KeyboardInterrupt:
        sys.stdout.write("\n")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
