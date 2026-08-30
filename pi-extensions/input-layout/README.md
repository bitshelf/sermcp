# pi.dev — DUT Statusline Extension

Pi extension that renders the sermcp's DUT serial state on the
editor border (`ctx.ui.setEditorComponent`), replacing the default header/
footer with a compact single-line layout:

```
┌─ ⠋ ────────────────────────────────────────────────────┐
│                                                         │
│                     ( editor content )                  │
│                                                         │
└─ model ───────────── ctx 81%/203k · ~/dev · ● pwm-rk3576:active ─┘
```

## Install

The repo root is a pi package (`package.json` with a `pi` manifest). From the
repo root:

```bash
pi install .
# or, from anywhere:
pi install /path/to/sermcp
```

This adds the local path to `~/.pi/agent/settings.json` (global). To install
project-locally instead, use `pi install . -l` (writes `.pi/settings.json`).

## Data source

The extension polls `.dut-serial/` state each render (2s) in this order:

1. `.dut-serial/inventory.json` — the MCP server's authoritative multi-DUT
   snapshot (each DUT's `dut_name` + `state`);
2. per-DUT `.dut-serial/<dut_name>/target-state` files;
3. legacy flat `.dut-serial/target-state` (name "serial").

The project dir is found by walking up from the CWD (`.target.jsonc` /
`.dut-serial`), or `TARGET_CONF`.

## State → symbol/color

Each DUT renders as `<symbol> <dut_name>:<state>` (e.g. `◐ pwm-rk3576:disconnected`).

| State | Symbol | Color |
|-------|--------|-------|
| active / booting / booted | `●` | success |
| uboot | `◆` | accent |
| crashed / DUT-off | `⚠` | error |
| disconnected / unknown | `◐` | error |
| stopped | `○` | muted |

Each segment carries a trailing space so the last segment is separated
from the closing border glyph (`─┘`) — no butt-joined rendering.
