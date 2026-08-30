/**
 * Input Layout — Custom editor border layout for pi
 *
 * ═══════════════════════════════════════════════════════════════════
 *  LAYOUT
 * ═══════════════════════════════════════════════════════════════════
 *
 *  ```
 *  ┌─ ⠋ ────────────────────────────────────────────────────────┐
 *  │                                                            │
 *  │                     ( editor content )                     │
 *  │                                                            │
 *  └─ model ───────────────── ctx %% · cwd · ● pwm-rk3576:active ───┘
 *  ```
 *
 *  ───────────────────────────────────────────────────────────────────
 *  TOP BORDER
 *  ───────────────────────────────────────────────────────────────────
 *    Left:   ⠋  Braille spinner (only while agent is working)
 *    Right:  (empty)
 *
 *  ───────────────────────────────────────────────────────────────────
 *  BOTTOM BORDER
 *  ───────────────────────────────────────────────────────────────────
 *    Left:   {provider}/{model} · {thinking_level}
 *             e.g. "opencode-go/glm-5.1 · high"
 *    Right:   ctx {percent}%/{window}k · {cwd} · {serial_state}
 *             e.g. "ctx 81%/203k · ~/dev/serial-debug · ● pwm-rk3576:active"
 *
 *    Border segments are separated by " · " (dimmed), truncated
 *    from right to left if the terminal is too narrow.
 *
 *  ───────────────────────────────────────────────────────────────────
 *  HIDDEN (replaced by Empty component)
 *  ───────────────────────────────────────────────────────────────────
 *    - Default header  (pi logo, keyboard shortcuts)
 *    - Default footer  (3 lines: cwd, stats, extension statuses)
 *    - Default working indicator (⠋ Working... line)
 *    → Header and footer are set to Empty; working indicator is
 *      disabled with setWorkingVisible(false). All info that was in
 *      the footer is now on the bottom border.
 *
 *  ───────────────────────────────────────────────────────────────────
 *  SERIAL STATE (read from .dut-serial/target-state; label = DUT name)
 *  ───────────────────────────────────────────────────────────────────
 *    ● <dut_name>:active        (green/success)  — target communicating normally
 *    ● <dut_name>:booting       (green/success)  — boot in progress
 *    ● <dut_name>:booted        (green/success)  — boot complete, shell ready
 *    ◆ <dut_name>:uboot          (cyan/accent)    — U-Boot prompt detected
 *    ⚠ <dut_name>:crashed       (red/error)     — kernel panic / BUG / Oops
 *    ⚠ <dut_name>:DUT-off       (red/error)     — no serial output, target hung
 *    ◐ <dut_name>:disconnected  (red/error)     — cannot connect to Dev Host ser2net
 *
 *    The file is polled every 2 seconds. Only appears when the project
 *    directory (or a parent) contains a .target.jsonc file (or TARGET_CONF
 *    points at one). A bare, leftover .dut-serial/ directory is NOT enough:
 *    without a config there is no DUT to talk about, so the segment — and the
 *    TARGET-ALERT injection — are suppressed entirely.
 *
 *  ───────────────────────────────────────────────────────────────────
 *  BACKEND (Rust MCP)
 *  ───────────────────────────────────────────────────────────────────
 *    The Rust sermcp server (`.mcp.json` / `.target.jsonc`)
 *    owns serial I/O, state tracking, and reconnection. This extension is
 *    display-only — it reads `.dut-serial/` state and renders it on the
 *    border. Do NOT use setStatus() for serial state (see note 1).
 *
 *  ───────────────────────────────────────────────────────────────────
 *  KEY DECISIONS & MAINTENANCE NOTES
 *  ───────────────────────────────────────────────────────────────────
 *    1. setFooter(() => new Empty()) hides the default 3-line pi footer.
 *       The bottom border replaces it. Do NOT use setStatus() for serial
 *       state — it would re-add the default footer's extension-status line.
 *
 *    2. setWorkingVisible(false) hides the built-in "⠋ Working..." line.
 *       The spinner is rendered on the top border instead.
 *
 *    3. setHeader(() => new Empty()) hides the pi logo/shortcuts header.
 *       If you want it back, remove that line.
 *
 *    4. Serial state file path: {project_dir}/.dut-serial/target-state
 *       Project dir is found by walking up from cwd looking for .target.jsonc.
 *
 *    5. ctx usage format: "ctx {percent}%/{window}k"
 *       e.g. "ctx 81%/203k" means 81% of 203k context window used.
 *
 *    6. fitBorder(left, right, width, borderFn) truncates from outside
 *       inward (right first, then left) when the terminal is too narrow.
 *
 *  /reload after modifying this file.
 */

import {
  CustomEditor,
  type ExtensionAPI,
  type ExtensionContext,
  type KeybindingsManager,
} from "@earendil-works/pi-coding-agent";
import type { Component, EditorTheme, TUI } from "@earendil-works/pi-tui";
import { truncateToWidth, visibleWidth } from "@earendil-works/pi-tui";
import * as fs from "node:fs";
import * as path from "node:path";

// ── Serial state mapping ─────────────────────────────────────────────

type StateColor = "success" | "error" | "warning" | "accent" | "muted";

const STATE_MAP: Record<string, { symbol: string; label: string; color: StateColor }> = {
  active:       { symbol: "●", label: "active",       color: "success" },
  booting:      { symbol: "●", label: "booting",      color: "success" },
  booted:       { symbol: "●", label: "booted",       color: "success" },
  uboot:        { symbol: "◆", label: "uboot",        color: "accent"  },
  crashed:      { symbol: "⚠", label: "crashed",      color: "error"   },
  "DUT-off":    { symbol: "⚠", label: "DUT-off",      color: "error"   },
  disconnected: { symbol: "◐", label: "disconnected", color: "error"   },
  unknown:      { symbol: "◐", label: "disconnected", color: "error"   },
  stopped:      { symbol: "○", label: "stopped",      color: "muted"   },
};

/** One DUT segment, e.g. `◐ pwm-rk3576:disconnected ` (trailing space). */
function stateSegment(state: string, name: string): { text: string; color: StateColor } | null {
  const info = STATE_MAP[state];
  if (!info) return null;
  return { text: `${info.symbol} ${name}:${info.label} `, color: info.color };
}

/** True when the project really has a DUT config (.target.jsonc / TARGET_CONF). */
function hasTargetConfig(projectDir: string | null): boolean {
  const env = process.env.TARGET_CONF;
  if (env && fs.existsSync(env)) return true;
  return projectDir !== null && fs.existsSync(path.join(projectDir, ".target.jsonc"));
}

/**
 * Project dir, but only for configured projects. Returns null when no
 * .target.jsonc exists so callers render nothing instead of a bogus
 * `◐ serial:disconnected` from a stale .dut-serial/ directory.
 */
function findConfiguredProjectDir(cwd: string): string | null {
  const dir = findProjectDir(cwd);
  return hasTargetConfig(dir) ? dir : null;
}

function findProjectDir(cwd: string): string | null {
  let d = cwd;
  const env = process.env.TARGET_CONF;
  if (env && fs.existsSync(env)) return path.dirname(path.resolve(env));
  let runtimeFallback: string | null = null;
  while (true) {
    if (fs.existsSync(path.join(d, ".target.jsonc"))) return d;
    // Remember a runtime-only project, but keep walking in case cwd is inside
    // .dut-serial and the real project configuration is in an ancestor.
    if (runtimeFallback === null && fs.existsSync(path.join(d, ".dut-serial"))) {
      runtimeFallback = d;
    }
    const parent = path.dirname(d);
    if (parent === d) break;
    d = parent;
  }
  return runtimeFallback;
}

type DutStatus = { dut_name: string; state: string; plain?: string };

/** Multi-DUT state reader (inventory.json → per-DUT target-state → flat). */
function readDutStatuses(projectDir: string): DutStatus[] {
  const dutSerial = path.join(projectDir, ".dut-serial");
  if (!fs.existsSync(dutSerial)) return [];

  // 1. inventory.json — authoritative multi-DUT state written by the MCP / dutabo.
  try {
    const invPath = path.join(dutSerial, "inventory.json");
    if (fs.existsSync(invPath)) {
      const inv = JSON.parse(fs.readFileSync(invPath, "utf-8"));
      const out: DutStatus[] = [];
      for (const dut of inv.duts ?? []) {
        if (!dut.dut_name) continue;
        out.push({
          dut_name: String(dut.dut_name),
          state: String(dut.state ?? ""),
          plain: dut.state_plain ? String(dut.state_plain) : undefined,
        });
      }
      if (out.length > 0) return out;
    }
  } catch { /* ignore parse errors */ }

  // 2. Per-DUT state files: .dut-serial/<dut_name>/target-state
  try {
    const skip = new Set(["default", "notifications"]);
    const out: DutStatus[] = [];
    const entries = fs.readdirSync(dutSerial, { withFileTypes: true });
    for (const e of entries) {
      if (!e.isDirectory() || skip.has(e.name)) continue;
      const f = path.join(dutSerial, e.name, "target-state");
      if (!fs.existsSync(f)) continue;
      out.push({ dut_name: e.name, state: fs.readFileSync(f, "utf-8").trim() });
    }
    if (out.length > 0) return out;
  } catch { /* ignore read errors */ }

  // 3. Legacy flat state file: .dut-serial/target-state
  try {
    const f = path.join(dutSerial, "target-state");
    if (fs.existsSync(f)) {
      return [{ dut_name: "serial", state: fs.readFileSync(f, "utf-8").trim() }];
    }
  } catch { /* ignore read errors */ }

  return [];
}

// ── Helpers ──────────────────────────────────────────────────────────

function fitBorder(
  left: string, right: string, width: number,
  borderFn: (text: string) => string,
): string {
  if (width <= 0) return "";
  if (width === 1) return borderFn("─");
  let l = left, r = right;
  while (2 + visibleWidth(l) + visibleWidth(r) + 3 > width && visibleWidth(r) > 0)
    r = truncateToWidth(r, Math.max(0, visibleWidth(r) - 1), "");
  while (2 + visibleWidth(l) + visibleWidth(r) + 3 > width && visibleWidth(l) > 0)
    l = truncateToWidth(l, Math.max(0, visibleWidth(l) - 1), "");
  const gap = Math.max(0, width - 2 - visibleWidth(l) - visibleWidth(r));
  return `${borderFn("─")}${l}${borderFn("─".repeat(gap))}${r}${borderFn("─")}`;
}

function shortCwd(cwd: string): string {
  const home = process.env.HOME;
  return home && cwd.startsWith(home) ? `~${cwd.slice(home.length)}` : cwd;
}

function ctxStr(ctx: ExtensionContext): string {
  const u = ctx.getContextUsage();
  const w = u?.contextWindow ?? ctx.model?.contextWindow;
  if (!w || !u || u.percent == null) return " ctx ?";
  return ` ctx ${Math.round(u.percent)}%/${(w / 1000).toFixed(0)}k`;
}

class Empty implements Component {
  render(): string[] { return []; }
  invalidate(): void {}
}

// ── Extension ───────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {
  let working = false;
  let si = 0;
  let timer: ReturnType<typeof setInterval> | undefined;
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  let tui: TUI | undefined;
  let currentProjectDir: string | null = null;
  const frames = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];

  const stop = () => { if (timer) { clearInterval(timer); timer = undefined; } };

  pi.on("agent_start", () => {
    working = true; stop();
    timer = setInterval(() => { si = (si + 1) % frames.length; tui?.requestRender(); }, 80);
    tui?.requestRender();
  });
  pi.on("agent_end", () => { working = false; stop(); tui?.requestRender(); });
  pi.on("session_shutdown", () => { stop(); if (pollTimer) clearInterval(pollTimer); tui = undefined; });

  // BeforeAgentStart: surface the worst DUT state as a TARGET-ALERT.
  pi.on("before_agent_start", async (event, ctx) => {
    const projectDir = findConfiguredProjectDir(ctx.cwd);
    if (!projectDir) return;

    const statuses = readDutStatuses(projectDir);
    if (statuses.length === 0) return;

    // Pick the most critical state across all DUTs.
    const severity: Record<string, number> = {
      crashed: 4,
      "DUT-off": 3,
      disconnected: 2,
      stopped: 1,
    };
    let worst: DutStatus | null = null;
    let worstScore = 0;
    for (const s of statuses) {
      const effective = s.state || "stopped";
      let score = 0;
      for (const [key, value] of Object.entries(severity)) {
        if (effective === key || effective.startsWith(key)) {
          score = Math.max(score, value);
        }
      }
      if (score > worstScore) {
        worstScore = score;
        worst = s;
      }
    }
    if (!worst) return;

    const state = worst.state;
    const label = statuses.length > 1 ? ` [${worst.dut_name}]` : "";
    let alert = "";

    if (!state || state === "stopped") {
      alert =
        "[TARGET] MCP serial server is not running. " +
        "Call any MCP tool (e.g. serial_get_state) to start it.";
    } else if (state.startsWith("DUT-off")) {
      alert =
        `[TARGET-ALERT]${label} DUT-off — no serial output for extended period. ` +
        'Try serial_send_command("echo ping") or serial_reset().';
    } else if (state === "disconnected") {
      alert =
        `[TARGET-ALERT]${label} Serial connection lost. ` +
        "Check Dev Host and ser2net, then call any MCP tool to reconnect.";
    } else if (state === "crashed") {
      alert =
        `[TARGET-ALERT]${label} Kernel crash detected! ` +
        'Run serial_get_logs(pattern="panic|BUG|Oops|Call trace") to see details.';
    }

    if (alert) {
      return { systemPrompt: event.systemPrompt + "\n\n" + alert };
    }
  });

  pi.on("session_start", (_event, ctx) => {
    currentProjectDir = findConfiguredProjectDir(ctx.cwd);

    // Hide default header and footer; spinner goes on border instead
    ctx.ui.setHeader(() => new Empty());
    ctx.ui.setWorkingVisible(false);
    ctx.ui.setFooter(() => new Empty());

    // Re-render every 2s to pick up serial state changes. Re-resolve the
    // project each tick so a config created mid-session (dutabo init) starts
    // showing state, and a removed one stops.
    pollTimer = setInterval(() => {
      currentProjectDir = findConfiguredProjectDir(ctx.cwd);
      tui?.requestRender();
    }, 2000);

    class FSEditor extends CustomEditor {
      constructor(t: TUI, theme: EditorTheme, kb: KeybindingsManager) {
        super(t, theme, kb, { paddingX: 0 });
        tui = t;
      }
      render(w: number): string[] {
        const lines = super.render(w);
        if (lines.length < 2) return lines;
        const thm = ctx.ui.theme;
        const model = ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : "no model";
        const think = pi.getThinkingLevel?.() ?? "off";
        const thinkStr = think === "off" ? "" : ` · ${think}`;

        // ── Top border: spinner left, nothing right ──
        const tLeft = working ? thm.fg("accent", ` ${frames[si]} `) : "";
        lines[0] = fitBorder(tLeft, "", w, (s) => this.borderColor(s));

        // ── Bottom border left: model · thinking ──
        const bLeft = thm.fg("dim", ` ${model}${thinkStr} `);

        // ── Bottom border right: ctx% · cwd · ◐ pwm-rk3576:disconnected ──
        const rightParts: string[] = [];
        rightParts.push(thm.fg("muted", ctxStr(ctx)));
        rightParts.push(thm.fg("muted", shortCwd(ctx.cwd)));
        if (currentProjectDir) {
          for (const dut of readDutStatuses(currentProjectDir)) {
            const seg = stateSegment(dut.state, dut.dut_name);
            if (seg) {
              rightParts.push(thm.fg(seg.color, seg.text));
            } else {
              rightParts.push(thm.fg("dim", `◐ ${dut.dut_name}:${dut.state} `));
            }
          }
        }
        const bRight = rightParts.join(thm.fg("dim", " · "));

        lines[lines.length - 1] = fitBorder(bLeft, bRight, w, (s) => this.borderColor(s));
        return lines;
      }
    }

    ctx.ui.setEditorComponent((t, theme, kb) => new FSEditor(t, theme, kb));
  });
}
