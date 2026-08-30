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

The extension reads `.dut-serial/inventory.json` every 2 seconds for the
server's authoritative multi-DUT snapshot (`dut_name` and `state`).

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
