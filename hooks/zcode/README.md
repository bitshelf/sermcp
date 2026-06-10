# ZCode Integration (remote development)

ZCode support rides on the same hook scripts as Claude Code
(`hooks/claude/*.py`) — ZCode's hook payloads, exit-code semantics, and
output JSON are Claude-Code-compatible. No script fork, no drift.

## Why this works over remote development

In ZCode remote development the **agent process runs on the workspace
host** — the machine that reaches ser2net, the dev hosts, and the relays.
Workspace-scope configuration (`<repo>/.zcode/config.json`) is read and
executed there, so the stdio MCP server, the auto-started HTTP MCP, and
every hook run on the dev host next to the hardware. The laptop client
needs nothing installed.

## What is wired

This repo ships a workspace-scope [`.zcode/config.json`](../../.zcode/config.json)
(pick it up by restarting the session; inspect under **Settings → MCP**):

| Piece | Source | What ZCode gets |
|-------|--------|-----------------|
| MCP server `sermcp` | `mcp.servers` → stdio `sermcp` | All 28 `serial_*` tools, auto-connected at session start |
| `SessionStart` | `hooks/claude/session-start.py` | Project detect, owner claim, auto-start per-DUT HTTP MCP |
| `UserPromptSubmit` | `hooks/claude/user-prompt-submit.py` | Multi-DUT state alerts, owner-only auto-restart |
| `PreToolUse` (matcher `Bash`) | `hooks/claude/pre-tool-use.py` | Block raw `nc`/`tio`/`screen`/`dutabo serial` from the agent |

## One-time setup (per remote host)

ZCode **configuration files do not expand `${...}` templates** (plugin MCP
servers are the exception), so the server entry uses a bare command
resolved from `PATH`:

```bash
rustup target add x86_64-unknown-linux-musl
cargo install --git https://github.com/bitshelf/sermcp --locked --target x86_64-unknown-linux-musl
# → ~/.cargo/bin/sermcp (also provides dutabo)
command -v sermcp   # must resolve before the session starts
```

If the binary lives outside `PATH`, override the entry in the user scope
(`~/.zcode/cli/config.json` → `mcp.servers.sermcp` with an absolute
`command`; user overrides workspace for a same-named server).

## Other target projects

For a project that only has `.target.jsonc` (no clone of this repo), copy
the example as its workspace config:

```bash
mkdir -p /path/to/project/.zcode
cp hooks/zcode/config.example.json /path/to/project/.zcode/config.json
```

The hook commands reference this checkout through shell `$HOME` — not
`${ZCODE_PROJECT_DIR}`, because in the target project that variable
resolves to the TARGET's root, which contains no `hooks/`. Adjust the
path if this repo is checked out elsewhere.

## Event mapping vs Claude Code

| Claude Code | ZCode | Note |
|-------------|-------|------|
| `SessionStart` | `SessionStart` | identical |
| `UserPromptSubmit` | `UserPromptSubmit` | identical |
| `PreToolUse` (`Bash`) | `PreToolUse` (matcher `Bash`) | identical |
| `SessionEnd` | **unsupported** | see cleanup below |
| `StatusLine` (`statusline.py`) | **unsupported** | use a terminal watcher |

### Cleanup without `SessionEnd`

ZCode has no session-end event, and its `Stop` event fires on **every
response end** — wiring `session-stop.py` there would kill the engine each
turn, so it is deliberately not wired. Cleanup falls back to two existing
mechanisms:

1. The detached HTTP MCP self-exits after 10 minutes without HTTP/WS
   activity (`--idle-timeout` default).
2. Ownership liveness: `owner.json` records the claiming pid; once it is
   dead, the next session's claim check takes over. Guests never
   restart/kill anything.

### Statusline substitute

ZCode has no statusline hook (its seven events do not include one), so
`statusline.py` cannot be ported. The event-driven equivalent is a watcher
in the integrated terminal (process runs remotely, renders locally):

```bash
dutabo status --watch --dut <board_name>
```

The Rust `notify`/inotify path pushes state changes with no polling; the
Python fallback is `python3 hooks/codex/serial-status.py --watch`.

## Skills / commands / subagent

Workspace config covers MCP + hooks only. The repo's `skills/`
(`serial-debug`, `embedded-debug`), `commands/debug-boot.md`, and
`agents/serial-boot-monitor.md` follow plugin-layout conventions, so they
are discovered when the repo is installed as a ZCode plugin (local-directory
marketplace; the `.claude-plugin/` manifest is recognized as a compat
name, and `hooks/hooks.json` contributes the same hooks — except the
unsupported `SessionEnd`, which is simply skipped).

## Ownership / multi-agent

Because the hook scripts are shared, the ownership contract is identical
across agents: the first session on a project claims `.dut-serial/owner.json`,
later sessions attach as guests (no restarts, no lock cleanup, guest
markers in display), and `dutabo serial` remains an explicit takeover that
is always allowed. A ZCode session and a Claude Code session on the same
project coordinate through the same files.

## Troubleshooting

- **Hooks never fire** — configuration-file hooks are disabled by default;
  `hooks.enabled: true` must be present (this repo's config has it).
  Plugin-contributed hooks enable the runner automatically.
- **Server `failed` with `ENOENT`** — `sermcp` is not on the
  spawn `PATH`; install it or add a user-scope absolute-path override.
- **Silently dropped config** — both the MCP server entry and the hooks
  block use strict schemas; unknown keys cause silent drops. Edit only the
  documented fields.
- **Run a hook by hand** — hooks read a Claude-compatible JSON payload on
  stdin and return 0 (pass) / 2 (block):
  ```bash
  echo '{"session_id":"t","cwd":"'"$PWD"'"}' | python3 hooks/claude/session-start.py
  echo '{"tool_name":"Bash","tool_input":{"command":"nc host 30001"}}' | python3 hooks/claude/pre-tool-use.py
  ```
- **Observe live** — hook runs (fired/timed-out/blocked, duration, stderr
  preview) are recorded in the ZCode log.
