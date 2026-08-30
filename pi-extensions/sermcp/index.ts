/**
 * sermcp — pi integration for the Rust sermcp MCP server.
 *
 * ═══════════════════════════════════════════════════════════════════
 *  WHY THIS EXTENSION IS SMALL
 * ═══════════════════════════════════════════════════════════════════
 *  pi >= 1.0 ships built-in MCP support (see docs/mcp.md). Calling
 *  `pi.registerMcpServer()` is enough to spawn the stdio server, perform
 *  the MCP handshake, register every advertised tool as
 *  `mcp__sermcp__<tool>`, reconnect a dropped connection, and list the
 *  server in `/mcp`. This extension therefore contains no JSON-RPC client
 *  and no npm dependencies. It only supplies what the built-in client
 *  cannot know:
 *
 *    1. Project discovery — walk up from the session directory for
 *       `.target.jsonc` (or honor `$TARGET_CONF`). No config means no
 *       registration, so unrelated projects stay untouched.
 *    2. Binary discovery — the sermcp executable is not necessarily on
 *       `$PATH`, so the same order the other harness integrations use
 *       applies (see below).
 *    3. Registration with `exposure: "direct"`, so the model sees the
 *       serial tools in its tool declarations without a tool-search or
 *       codemode step, and a generous per-request `timeout` for reset and
 *       boot waits (the server owns the real per-tool deadlines).
 *    4. `serial_mcp_diagnostics` — an extension tool (not an MCP tool)
 *       that reports binary, project, and registration state when the
 *       serial tools are missing.
 *
 * ═══════════════════════════════════════════════════════════════════
 *  MULTIPLE SESSIONS IN ONE PROJECT
 * ═══════════════════════════════════════════════════════════════════
 *  Every pi session registers the same stdio command. The Rust server
 *  keeps one engine per project: the first process owns it, and a later
 *  stdio client is transparently bridged into the running instance over
 *  its loopback HTTP endpoint (read-only guest), so no session needs
 *  owner/guest bookkeeping.
 *
 * ═══════════════════════════════════════════════════════════════════
 *  BINARY DISCOVERY ORDER
 * ═══════════════════════════════════════════════════════════════════
 *   1. $SERMCP_BIN          (explicit override)
 *   2. project `.mcp.json` / `.pi/mcp.json` mcpServers["sermcp"].command
 *   3. PATH lookup `sermcp`
 *   4. ~/.local/bin/sermcp  (standard install location)
 *   5. project bin/sermcp
 *   6. package-local bin/sermcp (plugin tarball layout)
 *
 *  Requires pi >= 1.0; on an older pi the extension reports that the
 *  built-in MCP support is missing instead of failing silently.
 *
 *  /reload after modifying this file.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { TSchema } from "typebox";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const SERVER_NAME = "sermcp";

/**
 * Client-side backstop in seconds. The server owns the real deadlines, but
 * `serial_reset(wait_boot=true)` waits for kernel and login prompts across
 * retries, so the built-in default (60s) is far too small.
 *
 * This is a guard, not a server limit: a call that passes a larger explicit
 * timeout (`serial_exec.timeout`, `serial_reset.failure_retry`) keeps
 * running on the server after pi gives up on the request. The defaults stay
 * well inside this value (a bounded reset is ≈545s worst case).
 */
const REQUEST_TIMEOUT_SECONDS = 1800;

const INSTALL_HINT =
  "install via: curl -L https://github.com/bitshelf/sermcp/releases/latest/download/sermcp-$(uname -m) -o ~/.local/bin/sermcp && chmod +x ~/.local/bin/sermcp";

// ── Project / binary discovery ─────────────────────────────────────

/** Walk up from cwd looking for .target.jsonc (or TARGET_CONF override). */
function findProjectDir(cwd: string): string | null {
  const env = process.env.TARGET_CONF;
  if (env && fs.existsSync(env)) return path.dirname(path.resolve(env));
  let d = cwd;
  let runtimeFallback: string | null = null;
  while (true) {
    if (fs.existsSync(path.join(d, ".target.jsonc"))) return d;
    // Remember a runtime-only project, but keep walking in case cwd is inside
    // .dut-serial and the real project configuration is in an ancestor.
    if (
      runtimeFallback === null &&
      fs.existsSync(path.join(d, ".dut-serial"))
    ) {
      runtimeFallback = d;
    }
    const parent = path.dirname(d);
    if (parent === d) break;
    d = parent;
  }
  return runtimeFallback;
}

/** The `.target.jsonc` that makes a directory a sermcp project, if any. */
function targetConfigPath(projectDir: string | null): string | null {
  const env = process.env.TARGET_CONF;
  if (env && fs.existsSync(env)) return path.resolve(env);
  if (!projectDir) return null;
  const local = path.join(projectDir, ".target.jsonc");
  return fs.existsSync(local) ? local : null;
}

/** Expand ${VAR} / ${HOME} in an mcp.json command string. */
function expandEnv(s: string): string {
  return s.replace(/\$\{([A-Za-z_][A-Za-z0-9_]*)\}/g, (_m, name: string) => {
    const v = name === "HOME" ? os.homedir() : process.env[name];
    return v ?? "";
  });
}

/** Read the `sermcp` command from a project MCP config, if present. */
function mcpJsonCommand(projectDir: string | null): string | undefined {
  if (!projectDir) return undefined;
  for (const file of [".mcp.json", path.join(".pi", "mcp.json")]) {
    try {
      const raw = fs.readFileSync(path.join(projectDir, file), "utf-8");
      const cfg = JSON.parse(raw) as {
        mcpServers?: Record<string, { command?: string }>;
      };
      const cmd = cfg.mcpServers?.[SERVER_NAME]?.command;
      if (cmd) return expandEnv(cmd);
    } catch {
      /* missing or unreadable config: try the next location */
    }
  }
  return undefined;
}

function fileExistsAndExecutable(p: string): boolean {
  try {
    fs.accessSync(p, fs.constants.X_OK);
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}

function findOnPath(name: string): string | undefined {
  const dirs = (process.env.PATH ?? "").split(path.delimiter);
  for (const dir of dirs) {
    if (!dir) continue;
    const candidate = path.join(dir, name);
    if (fileExistsAndExecutable(candidate)) return candidate;
  }
  return undefined;
}

/** Full discovery: env → project MCP config → PATH → ~/.local/bin → project → package. */
function findBinary(projectDir: string | null): string | undefined {
  const env = process.env.SERMCP_BIN;
  if (env && fileExistsAndExecutable(env)) return env;

  const mcpCmd = mcpJsonCommand(projectDir);
  if (mcpCmd && fileExistsAndExecutable(mcpCmd)) return mcpCmd;

  const inPath = findOnPath("sermcp");
  if (inPath) return inPath;

  const homeLocal = path.join(os.homedir(), ".local", "bin", "sermcp");
  if (fileExistsAndExecutable(homeLocal)) return homeLocal;

  if (projectDir) {
    const projBin = path.join(projectDir, "bin", "sermcp");
    if (fileExistsAndExecutable(projBin)) return projBin;
  }

  // Package-local bin/ (plugin tarball layout: <pkg>/bin/sermcp).
  const pkgBin = path.join(__dirname, "..", "..", "bin", "sermcp");
  if (fileExistsAndExecutable(pkgBin)) return pkgBin;

  return undefined;
}

// ── Extension ──────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {
  /** Project whose server is registered in this session, if any. */
  let registeredDir: string | null = null;
  /** Last directory that had a config but could not be wired up. */
  let attemptedDir: string | null = null;
  let configPath: string | null = null;
  let binary: string | undefined;
  let lastError: string | undefined;

  const hasBuiltinMcp = typeof pi.registerMcpServer === "function";

  const log = (line: string, projectDir: string | null) => {
    // Only ever write inside a CONFIGURED project. Logging from an
    // unconfigured cwd used to mkdir a stray `.dut-serial/` (e.g. ~/.dut-serial
    // when pi runs in ~/.pi/agent), which the statusline extension then
    // mistook for a project and rendered as `◐ serial:disconnected`.
    if (!projectDir) return;
    try {
      fs.mkdirSync(path.join(projectDir, ".dut-serial"), { recursive: true });
      fs.appendFileSync(
        path.join(projectDir, ".dut-serial", "pi-extension.log"),
        `${new Date().toISOString()} ${line}\n`,
      );
    } catch {
      /* logging is best-effort */
    }
  };

  /**
   * Wire the sermcp MCP server into pi for the project enclosing `cwd`.
   * Safe to call repeatedly: a project that is already registered is left
   * alone, and an unconfigured directory is ignored.
   */
  function tryRegister(cwd: string): void {
    const projectDir = findProjectDir(cwd);
    const config = targetConfigPath(projectDir);
    if (!projectDir || !config) return; // not a sermcp project
    if (registeredDir === projectDir) return; // already wired

    attemptedDir = projectDir;
    configPath = config;
    binary = findBinary(projectDir);

    if (!binary) {
      lastError = "sermcp binary not found";
      log(`${lastError} (${INSTALL_HINT})`, projectDir);
      return;
    }
    if (!hasBuiltinMcp) {
      lastError = "this pi version has no built-in MCP support (pi >= 1.0 required)";
      log(lastError, projectDir);
      return;
    }

    try {
      pi.registerMcpServer(SERVER_NAME, {
        command: binary,
        cwd: projectDir,
        env: { TARGET_CONF: config },
        // Direct exposure: the serial tools are declared to the model
        // without an intermediate tool-search or codemode step.
        exposure: "direct",
        timeout: REQUEST_TIMEOUT_SECONDS,
        description:
          "Serial console for embedded DUTs: state, logs, shell and U-Boot commands, reset, power, button, and flash-plan tools.",
      });
      registeredDir = projectDir;
      lastError = undefined;
      log(`registered MCP server "${SERVER_NAME}" (${binary})`, projectDir);
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
      registeredDir = null;
      log(`registerMcpServer failed: ${lastError}`, projectDir);
    }
  }

  // Register while the extension loads, so the built-in MCP client connects
  // together with the mcp.json servers at session start.
  tryRegister(process.cwd());

  pi.on("session_start", (_event, ctx) => {
    tryRegister(ctx.cwd);
    if (!registeredDir && attemptedDir && lastError) {
      ctx.ui.notify(`sermcp: ${lastError}`, "warning");
    }
  });

  // `dutabo init` can create `.target.jsonc` after the session started; a
  // re-check here lets the next turn pick it up without /reload. A project
  // that had a config but failed discovery is not retried every turn — an
  // uninstalled binary needs /reload once it is installed.
  pi.on("before_agent_start", (_event, ctx) => {
    if (!registeredDir && attemptedDir === null) tryRegister(ctx.cwd);
  });

  pi.on("session_shutdown", () => {
    if (!registeredDir) return;
    try {
      pi.unregisterMcpServer(SERVER_NAME);
    } catch {
      /* session teardown closes the connection anyway */
    }
    registeredDir = null;
  });

  // Diagnostics tool — always registered so the agent can introspect.
  pi.registerTool({
    name: "serial_mcp_diagnostics",
    label: "serial_mcp_diagnostics",
    description:
      "Report how the sermcp MCP server is wired into this pi session: " +
      "the discovered binary, project config, registration state, and the " +
      "MCP tool names pi exposes. Use when serial tools are missing or a " +
      "serial_* call fails.",
    parameters: { type: "object" } as TSchema,
    async execute() {
      const prefix = `mcp__${SERVER_NAME}__`;
      const tools = pi
        .getAllTools()
        .map((tool) => tool.name)
        .filter((name) => name.startsWith(prefix))
        .sort();
      const lines = [
        `binary: ${binary ?? "(not found)"}`,
        `project_dir: ${attemptedDir ?? "(none — no .target.jsonc found)"}`,
        `config: ${configPath ?? "(none)"}`,
        `mcp_server: ${
          registeredDir
            ? `"${SERVER_NAME}" registered for ${registeredDir} (exposure: direct, timeout: ${REQUEST_TIMEOUT_SECONDS}s)`
            : "not registered"
        }`,
        `serial_tools (${tools.length}): ${
          tools.join(", ") || "(none — server not connected yet)"
        }`,
      ];
      if (lastError) lines.push(`last_error: ${lastError}`);
      if (!binary) lines.push(`install_hint: ${INSTALL_HINT}`);
      lines.push(
        "inspect: /mcp shows the server and its connection state; /reload re-runs discovery after installing the binary",
      );
      return {
        content: [{ type: "text", text: lines.join("\n") }],
        details: {},
      };
    },
  });
}
