/**
 * dsh-dut — sermcp × dsh (DeepSeek Harness) integration bundle.
 *
 * One Cordis package, two plugin rows dispatched by `config.role`:
 *
 *   role: 'statusline'  (row id `dut-statusline`)
 *     Watch <project>/.dut-serial/inventory.json (fs.watch / inotify —
 *     event-driven, NO polling: a state change re-renders within ms, an
 *     idle project costs zero CPU) and render DUT states on the dsh-tui
 *     statusline via the `tuiStatus` seam. This is the classic
 *     dsh-dut-statusline behavior, folded into the bundle.
 *
 *   role: 'mcp'  (row id `dut-mcp`) — the MCP's SERVER plugin row
 *     Project-aware MCP server bridge: discover the project, keep its
 *     sermcp HTTP server alive, connect over Streamable HTTP and
 *     register every `serial_*` tool as `mcp__<serverName>__<tool>` on
 *     `ctx.tools` — the dsh agent can drive the DUT serial console exactly
 *     like Claude Code and pi.dev do. Multiple DUTs / dev hosts are handled
 *     by the server itself (one server process, tools carry `dut_name`).
 *     The `mcp__<server>__<tool>` naming is dsh's server-plugin contract
 *     (the shape the official @deepseek-ai/dsh-mcp-client row and dsh-tui's
 *     /mcp panel use). The official client row is NOT used on purpose: it
 *     is an rc-line package whose runtime deps pin the rc dsh core — on an
 *     alpha-line profile pnpm would install a second, incompatible dsh
 *     core beside the TUI's, breaking at the next dsh/dsh-tui upgrade.
 *     This bridge imports only node builtins + @modelcontextprotocol/sdk,
 *     so dsh/dsh-tui upgrades never break it.
 *
 * Both roles share the same project-discovery/liveness/auto-start plumbing:
 *
 *   1. Project discovery walks up from the session workspace dir
 *      (.dut-serial/inventory.json or .target.jsonc; TARGET_CONF /
 *      TARGET_EXCLUDE semantics match the Python hooks). The workspace starts
 *      at DSH_TUI_WORKSPACE_TARGET / process CWD and follows
 *      `tui/session-switched` events (--resume, /workspace switches).
 *   2. Liveness gate: the inventory's mcp_pid must be alive
 *      (process.kill(pid, 0)); otherwise a 127.0.0.1 TCP probe of the HTTP
 *      port. A dead server inside a configured project triggers AUTO-START:
 *      spawn the project's server (`--http 127.0.0.1:<port>`, detached),
 *      rate-limited — mirrors the Claude Code session-start hook.
 *   3. Outside a configured project both rows stay dormant: no statusline
 *      contribution, no connection, no tool registration.
 *
 * Contracts:
 *   - No dsh-tui internal imports, no version pinning: only the stable
 *     `tuiStatus` seam / `tui/session-switched` event (statusline) and the
 *     `ctx.tools` service (bridge).
 *   - The bridge is honest about liveness: when the server is down the tools
 *     are unregistered (the model's tool set refreshes via tools/change);
 *     a stale registered tool that would fail every call is never kept.
 *   - All I/O, spawn, connect and register calls are fenced; failures only
 *     produce debug logs and never crash the TUI.
 */
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  appendFileSync,
  existsSync,
  readFileSync,
  readdirSync,
  realpathSync,
  watch,
} from 'node:fs'
import { homedir } from 'node:os'
import { basename, delimiter, dirname, join, resolve, sep } from 'node:path'
import net from 'node:net'

import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js'

export const name = 'dsh-dut'
/** Code-level dependencies: compositions without these services fail loudly
 *  at load instead of silently doing nothing. Both rows inject both — the
 *  services exist in every dsh-tui profile (tuiStatus from dsh-tui,
 *  tools from the base bundle). */
export const inject = ['tuiStatus', 'tools']

export const STATUSLINE_DEFAULTS = Object.freeze({
  role: 'statusline',
  /** Timeout (ms) of the TCP liveness probe used when mcp_pid is dead. */
  tcpTimeoutMs: 300,
  /** Auto-start the project's MCP server when no server answers and the
   *  startup dir is a configured project (.target.jsonc). `false` disables
   *  (then a dead server renders the disconnected fallback without
   *  spawning). */
  autoStart: true,
  /** Minimum gap between auto-start spawn attempts (ms). */
  autoStartIntervalMs: 60000,
  /** Delay before re-checking after a successful auto-start spawn (ms) —
   *  the server needs a moment to bind and rewrite inventory.json. */
  autoStartRetryMs: 3000,
  /** Explicit path to the sermcp binary; default: PATH lookup
   *  then the usual install dirs. */
  serverBinary: undefined,
  /** Debug: append per-event decisions as JSON lines to this path;
   *  DUT_STATUSLINE_DEBUG env takes precedence. Diagnostics only. */
  debugLogPath: process.env.DUT_STATUSLINE_DEBUG ?? undefined,
})

/** Backwards-compatible alias for the old dsh-dut-statusline config name. */
export const DEFAULTS = STATUSLINE_DEFAULTS

export const MCP_DEFAULTS = Object.freeze({
  role: 'mcp',
  /** Namespace for this server's model-facing tool names: tools register as
   *  `mcp__<serverName>__<rawName>`. Must match [A-Za-z0-9_-]{1,32}. */
  serverName: 'dut',
  /** Connection/bridge maintenance poll interval (ms). */
  pollIntervalMs: 2000,
  /** Timeout (ms) of the TCP liveness probe used when mcp_pid is dead. */
  tcpTimeoutMs: 300,
  /** Auto-start the project's MCP server when missing (same semantics as the
   *  statusline row). */
  autoStart: true,
  /** Minimum gap between auto-start spawn attempts (ms). */
  autoStartIntervalMs: 60000,
  /** Delay before re-polling after a successful auto-start spawn (ms). */
  autoStartRetryMs: 3000,
  /** Explicit path to the sermcp binary. */
  serverBinary: undefined,
  /** First reconnect delay after a lost connection (ms); doubles per
   *  consecutive failure up to reconnectMaxMs. */
  reconnectInitialMs: 500,
  /** Reconnect backoff ceiling (ms). */
  reconnectMaxMs: 30000,
  /** Timeout per tools/call invocation (ms). */
  toolCallTimeoutMs: 60000,
  /** Optional RegExp source: only tools whose raw name matches are
   *  registered (token saving; e.g. '^serial_(get|list)_' for read-only).
   *  `undefined`/empty registers everything. */
  toolPattern: undefined,
  /** Debug: append per-tick decisions as JSON lines to this path;
   *  DSH_DUT_DEBUG env takes precedence. Diagnostics only. */
  debugLogPath: process.env.DSH_DUT_DEBUG ?? undefined,
})

const INVENTORY_FILE = 'inventory.json'
const CONFIG_FILE = '.target.jsonc'
/** Directories never treated as project roots themselves (matches
 *  hooks/claude/inventory.py). */
const FORBIDDEN_ROOTS = new Set(['/tmp', '/var/tmp', '/dev/shm', '/run', '/proc', '/sys', '/dev'])
/** tuiStatus key: lowercase slug, plugin-namespaced. */
const KEY = 'dut-statusline'
/** Base of the streamable-http MCP endpoint on the server. */
const MCP_PATH = '/mcp'

// ── Project discovery (semantics aligned with hooks/claude/inventory.py) ──

function excludedPaths() {
  const raw = process.env.TARGET_EXCLUDE ?? ''
  return raw
    .split(':')
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => resolve(part))
}

export function isExcludedPath(dir, excludes = excludedPaths()) {
  const resolved = resolve(dir)
  return excludes.some((ex) => resolved === ex || resolved.startsWith(ex + sep))
}

export function checkDir(dir, excludes = excludedPaths()) {
  const resolved = resolve(dir)
  if (FORBIDDEN_ROOTS.has(resolved) || isExcludedPath(resolved, excludes)) return null
  if (existsSync(join(resolved, '.dut-serial', INVENTORY_FILE))) return resolved
  if (existsSync(join(resolved, CONFIG_FILE))) return resolved
  return null
}

/** Find the project root: TARGET_CONF override → CWD walk-up → null. */
export function findProjectDir(cwd = process.cwd(), excludes = excludedPaths()) {
  const override = process.env.TARGET_CONF
  if (override) {
    const p = resolve(override)
    if (!existsSync(p)) return null
    const dir = dirname(p)
    if (FORBIDDEN_ROOTS.has(dir) || isExcludedPath(dir, excludes)) return null
    return dir
  }
  let d = resolve(cwd)
  for (;;) {
    const found = checkDir(d, excludes)
    if (found) return found
    const parent = dirname(d)
    if (parent === d) break
    d = parent
  }
  return null
}

// ── Inventory read + liveness gate ─────────────────────────────────────────

/** Read and shape-check inventory.json; missing/corrupt/wrong version → null,
 *  never throws. */
export function loadInventory(projectDir) {
  try {
    const text = readFileSync(join(projectDir, '.dut-serial', INVENTORY_FILE), 'utf8')
    const inv = JSON.parse(text)
    if (inv === null || typeof inv !== 'object' || inv.schema_version !== 1) return null
    return inv
  } catch {
    return null
  }
}

export function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

/** Is the pid ACTUALLY a sermcp process? os.kill(pid, 0) only
 *  proves the pid exists — a recycled pid would make a stale inventory look
 *  live. Matches the executable identity instead, with the SAME two checks
 *  as the Python hooks (lib._is_embedded_server): /proc/<pid>/comm OR the
 *  argv[0] basename. Returns false on any read failure. */
export function pidIsMcpServer(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false
  try {
    const comm = readFileSync(`/proc/${pid}/comm`, 'utf8').trim()
    if (comm.includes('sermcp')) return true
    const argv0 = readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0')[0]
    if (argv0 !== '' && basename(argv0).startsWith('sermcp')) return true
    return false
  } catch {
    return false
  }
}

/** Full liveness gate for an inventory snapshot:
 *  1. identity — the recorded mcp_pid is alive AND actually a
 *     sermcp process (recycled-pid protection);
 *  2. freshness — when the server declared a heartbeat (heartbeat_secs),
 *     written_at_unix must be within 3 × heartbeat_secs: the server
 *     rewrites the inventory on that cadence even when idle, so a stale
 *     written_at means the server died (kill -9 leaves no cleanup hook).
 *     Snapshots without heartbeat_secs (dutabo list --json, legacy
 *     servers) fall back to the identity gate alone. */
export function inventoryLive(inv) {
  const pid = inv?.mcp_pid
  if (!Number.isInteger(pid) || pid <= 0) return false
  if (!pidAlive(pid) || !pidIsMcpServer(pid)) return false
  const heartbeat = inv.heartbeat_secs
  if (Number.isInteger(heartbeat) && heartbeat > 0) {
    const writtenAt = inv.written_at_unix
    if (!Number.isInteger(writtenAt)) return false
    if (Date.now() / 1000 - writtenAt > 3 * heartbeat) return false
  }
  return true
}

/** One short-timeout TCP probe of 127.0.0.1:<port> (liveness fallback when
 *  mcp_pid is missing or dead). */
export function portAlive(port, timeoutMs = 300) {
  return new Promise((resolve) => {
    if (!Number.isInteger(port) || port <= 0 || port > 65535) return resolve(false)
    let settled = false
    const done = (ok) => {
      if (settled) return
      settled = true
      socket.destroy()
      resolve(ok)
    }
    const socket = net.connect({ host: '127.0.0.1', port })
    socket.setTimeout(timeoutMs, () => done(false))
    socket.on('connect', () => done(true))
    socket.on('error', () => done(false))
  })
}

// ── MCP server auto-start (startup dir has .target.jsonc → bring it up) ───
//
// The port formulas below are the JS mirror of `sermcp/src/ports.rs`
// (`project_mcp_port` / `project_dut_mcp_port`), byte-identical to the
// session-start hook and the old dsh-dut-statusline plugin.

/** fs.realpathSync equivalent of ports.rs `canonical()`. */
export function canonicalDir(dir) {
  try {
    return realpathSync(dir)
  } catch {
    return dir
  }
}

function portFromDigest(hexDigest) {
  return 3001 + (parseInt(hexDigest.slice(0, 8), 16) % 99)
}

/** Project-scoped control port: md5(canonical(project_dir))[:8] % 99 + 3001. */
export function projectPort(projectDir) {
  const hex = createHash('md5').update(canonicalDir(projectDir)).digest('hex')
  return portFromDigest(hex)
}

/** Per-DUT HTTP port: md5(canonical(project_dir) + ":" + dut_name) → range. */
export function dutPort(projectDir, name) {
  const hex = createHash('md5')
    .update(canonicalDir(projectDir))
    .update(':')
    .update(String(name))
    .digest('hex')
  return portFromDigest(hex)
}

/** The port to probe/spawn for a project: inventory's recorded port first
 *  (the actual bound --http port), else the current DUT's port, else the
 *  first DUT's, else the project port. */
export function mcpPortFor(projectDir, inv) {
  const recorded = inv?.mcp_http_port
  if (Number.isInteger(recorded) && recorded > 0 && recorded <= 65535) return recorded
  const current =
    typeof inv?.current_dut === 'string'
      ? inv.current_dut
      : process.env.TARGET_DUT_NAME || undefined
  if (typeof current === 'string' && current !== '') return dutPort(projectDir, current)
  const first = Array.isArray(inv?.duts) ? inv.duts[0]?.dut_name : undefined
  if (typeof first === 'string' && first !== '') return dutPort(projectDir, first)
  return projectPort(projectDir)
}

/** Locate the sermcp binary: explicit override → PATH → the
 *  usual install dirs (same preference order as the session hook). */
export function resolveServerBinary(override) {
  if (typeof override === 'string' && override !== '') {
    return existsSync(override) ? override : null
  }
  const seen = new Set()
  const candidates = []
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    if (dir !== '') candidates.push(join(dir, 'sermcp'))
  }
  candidates.push(join(homedir(), '.local', 'bin', 'sermcp'))
  const cacheDir = join(homedir(), '.claude', 'plugins', 'cache', 'local-res', 'embedded-debug')
  let versions = []
  try {
    versions = readdirSync(cacheDir).sort().reverse()
  } catch {
    /* no cached plugin versions */
  }
  for (const version of versions) {
    candidates.push(join(cacheDir, version, 'bin', 'sermcp'))
  }
  candidates.push(join(homedir(), 'plugins', 'sermcp', 'bin', 'sermcp'))
  for (const candidate of candidates) {
    if (seen.has(candidate)) continue
    seen.add(candidate)
    if (existsSync(candidate)) return candidate
  }
  return null
}

/** argv for the spawned server (the session hook uses the same shape). */
export function serverCommand(port) {
  return ['--http', `127.0.0.1:${port}`]
}

/** Spawn env: inherit + TARGET_CONF/TARGET_DUT_NAME like the hook. */
export function serverEnv(projectDir, inv) {
  const dut =
    typeof inv?.current_dut === 'string'
      ? inv.current_dut
      : process.env.TARGET_DUT_NAME ||
        (Array.isArray(inv?.duts) ? inv.duts[0]?.dut_name : undefined)
  const env = { ...process.env, TARGET_CONF: join(projectDir, '.target.jsonc') }
  if (typeof dut === 'string' && dut !== '') env.TARGET_DUT_NAME = dut
  return env
}

/** Spawn the MCP server detached (survives the TUI). `spawnImpl` (also
 *  accepted as `config.spawnImpl`) is a test seam; the real default detaches
 *  and ignores stdio. Never throws. */
export function ensureServer({ projectDir, inv, port, config = {}, spawnImpl = spawn }) {
  const impl = config.spawnImpl ?? spawnImpl
  const binary = resolveServerBinary(config.serverBinary)
  if (binary === null) return { ok: false, reason: 'no sermcp binary found' }
  try {
    const child = impl(binary, serverCommand(port), {
      cwd: projectDir,
      env: serverEnv(projectDir, inv),
      detached: true,
      stdio: 'ignore',
    })
    child.unref?.()
    return { ok: true, pid: child.pid ?? null, port, binary }
  } catch (err) {
    return { ok: false, reason: err?.message ?? String(err) }
  }
}

// ── Statusline rendering ───────────────────────────────────────────────────

/**
 * Render per-DUT Rust preformatted `state_text` verbatim — the ANSI string
 * that the Claude Code statusline hook also renders (green ● active, red ✗
 * crashed, ...); the state→color mapping lives only in Rust. Empty
 * `state_text` (unknown/stopped) is skipped — never a made-up healthy
 * state. The caller derives both the plain fallback and the structured
 * segments from this string.
 */
export function renderStatus(inv, guest = false) {
  const duts = Array.isArray(inv?.duts) ? inv.duts : []
  const parts = []
  for (const dut of duts) {
    const rendered = guest ? dut?.guest_display?.text : dut?.state_text
    const text = typeof rendered === 'string' ? rendered.trim() : ''
    if (!text) continue
    parts.push(text)
  }
  return parts.join(' · ')
}

/** SGR foreground color codes → the TUI segment palette (Ink `ansi:`
 *  names). Rust's state_manager emits exactly these (31–37, 90–97);
 *  anything else renders as an uncolored run. */
const SGR_TO_COLOR = {
  '30': 'ansi:black', '31': 'ansi:red', '32': 'ansi:green', '33': 'ansi:yellow',
  '34': 'ansi:blue', '35': 'ansi:magenta', '36': 'ansi:cyan', '37': 'ansi:white',
  '90': 'ansi:blackBright', '91': 'ansi:redBright', '92': 'ansi:greenBright',
  '93': 'ansi:yellowBright', '94': 'ansi:blueBright', '95': 'ansi:magentaBright',
  '96': 'ansi:cyanBright', '97': 'ansi:whiteBright',
}

/**
 * Parse a Rust preformatted ANSI string into `{ text, fg? }` runs for
 * `tuiStatus.setStyled`. SGR resets (`m` = 0) clear the active color;
 * unknown codes keep the previous color; plain runs carry no `fg`.
 */
export function parseAnsiSegments(value) {
  const segments = []
  let color
  for (const part of String(value).split(/(\x1b\[[0-9;]*m)/)) {
    if (part === '') continue
    const m = part.match(/^\x1b\[([0-9;]*)m$/)
    if (m !== null) {
      const params = m[1]
      if (params === '' || params === '0') color = undefined
      else color = SGR_TO_COLOR[params] ?? color
      continue
    }
    segments.push(color === undefined ? { text: part } : { text: part, fg: color })
  }
  return segments
}

/** Strip ANSI SGR codes — the fallback for TUIs without `setStyled`, whose
 *  sanitizer would otherwise leak raw `[32m` text onto the statusline. */
export function stripAnsi(value) {
  return String(value).replace(/\x1b\[[0-9;]*m/g, '')
}

/** Honest fallback while the project's server is down: every known DUT reads
 *  `✗ <name>:disconnected` — same symbol the live server uses for a lost
 *  serial link, so the line never fabricates a healthy state and never
 *  renders blank inside a configured project. */
export function renderDisconnected(inv) {
  const duts = Array.isArray(inv?.duts) ? inv.duts : []
  const parts = []
  for (const dut of duts) {
    const name = typeof dut?.dut_name === 'string' ? dut.dut_name.trim() : ''
    if (name === '') continue
    // Red, matching the Rust state color for Disconnected (`\x1b[31m✗`).
    parts.push(`\x1b[31m✗ ${name}:disconnected\x1b[0m`)
  }
  return parts.join(' · ')
}

// ── Ownership (multi-agent isolation) ──────────────────────────────────────

const OWNER_FILE = 'owner.json'
const CLAIM_GRACE_SECS = 60

/** Read `.dut-serial/owner.json` (the first-opens-wins record written by the
 *  hooks); corrupt/missing → null. */
export function loadOwner(projectDir) {
  try {
    const obj = JSON.parse(readFileSync(join(projectDir, '.dut-serial', OWNER_FILE), 'utf8'))
    if (obj === null || typeof obj !== 'object' || obj.schema_version !== 1) return null
    return obj
  } catch {
    return null
  }
}

/** Owner liveness anchors on owner_mcp_pid; a fresh claim whose anchor has
 *  not been back-filled yet stays alive for CLAIM_GRACE_SECS. */
export function ownerIsAlive(owner) {
  if (owner === null || typeof owner !== 'object') return false
  if (pidAlive(owner.owner_mcp_pid)) return true
  if (owner.owner_mcp_pid == null) {
    const claimed = owner.claimed_at_unix
    if (typeof claimed === 'number' && Number.isFinite(claimed)) {
      return Date.now() / 1000 - claimed < CLAIM_GRACE_SECS
    }
  }
  return false
}

/** Select the visitor view only when a different live session owns it. */
export function isGuest(owner, myPid = process.pid) {
  if (!ownerIsAlive(owner)) return false
  if (owner.owner_session_id === `dsh:${myPid}`) return false
  return owner.owner_pid !== myPid
}

// ── MCP bridge: tool naming + definitions ──────────────────────────────────

/** Public model-facing name of one bridged tool: `mcp__<serverName>__<raw>`.
 *  serial_* raw names and a [A-Za-z0-9_-]{1,32} serverName always fit the
 *  DeepSeek function-name contract (≤64 chars, [A-Za-z0-9_-]), so no
 *  normalization/hash suffix is needed here. */
export function publicToolName(serverName, rawName) {
  return `mcp__${serverName}__${rawName}`
}

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

/** Extract a plain-text projection from an MCP callTool result
 *  ({ content: ContentBlock[] } or { toolResult }). Text blocks join with
 *  newlines; anything else is JSON-serialized rather than dropped. */
export function extractText(value) {
  if (isPlainObject(value) && Array.isArray(value.content)) {
    const parts = value.content.map((block) => {
      if (isPlainObject(block) && block.type === 'text') return String(block.text ?? '')
      try {
        return JSON.stringify(block)
      } catch {
        return String(block)
      }
    })
    return parts.join('\n')
  }
  if (isPlainObject(value) && 'toolResult' in value) {
    try {
      return JSON.stringify(value.toolResult)
    } catch {
      return '(no output)'
    }
  }
  try {
    return JSON.stringify(value)
  } catch {
    return '(no output)'
  }
}

/**
 * Build one harness ToolDefinition for a raw MCP tool. The execute sends an
 * uncached `tools/call` with the RAW name (never the public one), honors the
 * per-call timeout, and throws on `isError` so the ToolRuntime renders the
 * failure as an error result. Returns the canonical `{ content,
 * structuredContent? }` value; `output.render` projects text for the model.
 */
export function createDefinition(client, publicName, rawName, description, parameters, timeoutMs = 60000) {
  return {
    name: publicName,
    description: typeof description === 'string' ? description : '',
    parameters: isPlainObject(parameters) ? parameters : { type: 'object', properties: {} },
    output: {
      schema: {
        type: 'object',
        properties: { content: { type: 'array', items: {} } },
        required: ['content'],
        additionalProperties: false,
      },
      render(_args, value) {
        return [{ type: 'text', text: extractText(value) }]
      },
    },
    async execute(args) {
      // SDK ≥1.30 signature: callTool(params, resultSchema?, options?).
      // `undefined` keeps the default CallToolResultSchema; options carry the
      // per-call timeout.
      const result = await client.callTool(
        { name: rawName, arguments: isPlainObject(args) ? args : {} },
        undefined,
        { timeout: timeoutMs },
      )
      const content = isPlainObject(result) && Array.isArray(result.content)
        ? result.content
        : [{ type: 'text', text: extractText(result) }]
      if (result?.isError === true) throw new Error(extractText({ content }))
      const value = { content }
      if (isPlainObject(result) && result.structuredContent !== undefined) {
        value.structuredContent = result.structuredContent
      }
      return value
    },
  }
}

/**
 * Fetch `tools/list` and register every (pattern-matching) tool on
 * `ctx.tools`, returning the disposer map keyed by public name. Any
 * registration failure rolls back the partial generation so the registry is
 * never left half-populated.
 */
export async function syncTools(client, ctx, { serverName, toolPattern, toolCallTimeoutMs = 60000 }) {
  const response = await client.listTools()
  const tools = Array.isArray(response?.tools) ? response.tools : []
  const regex = typeof toolPattern === 'string' && toolPattern !== '' ? new RegExp(toolPattern) : null
  const definitions = new Map()
  for (const tool of tools) {
    const rawName = tool?.name
    if (typeof rawName !== 'string' || rawName === '') continue
    if (regex !== null && !regex.test(rawName)) continue
    const publicName = publicToolName(serverName, rawName)
    if (definitions.has(publicName)) {
      throw new Error(`dsh-dut: server listed tool "${rawName}" more than once — invalid tool list`)
    }
    definitions.set(
      publicName,
      createDefinition(client, publicName, rawName, tool.description, tool.inputSchema, toolCallTimeoutMs),
    )
  }
  const disposers = new Map()
  try {
    for (const [publicName, definition] of definitions) {
      disposers.set(publicName, ctx.tools.register(definition))
    }
  } catch (error) {
    for (const dispose of disposers.values()) dispose()
    throw error
  }
  return disposers
}

// ── Plugin body: role dispatch ─────────────────────────────────────────────
export function apply(ctx, config = {}) {
  if (config.role === 'mcp') return applyMcp(ctx, config)
  return applyStatusline(ctx, config)
}

// ── Statusline role (classic dsh-dut-statusline behavior) ──────────────────

/**
 * @param {object} ctx Cordis context (injects tuiStatus, tools)
 * @param {object} [config] row config: { role: 'statusline', ... }
 */
export function applyStatusline(ctx, config = {}) {
  const opts = { ...STATUSLINE_DEFAULTS, ...config }
  const status = ctx.tuiStatus
  const log = (msg) => {
    try {
      ctx.logger?.debug?.(msg)
    } catch {
      /* A statusline plugin must never crash on a logging failure */
    }
  }

  let latestDispose = null
  let offSwitch = null
  let watcher = null
  let retryTimer = null
  let lastSpawnAt = 0
  let ticking = false
  let lastText = '\u0000' // sentinel: force the first write (including a first clear)
  let lastSegments = null
  // Session workspace: launch override > process CWD; then `tui/session-switched`
  // (--resume, /workspace switches, rewind) takes over so we never stare at
  // the launch CWD forever.
  let workspaceCwd = process.env.DSH_TUI_WORKSPACE_TARGET || process.cwd()

  const dbg = (entry) => {
    if (!opts.debugLogPath) return
    try {
      appendFileSync(
        opts.debugLogPath,
        JSON.stringify({ ts: new Date().toISOString(), role: 'statusline', ...entry }) + '\n',
      )
    } catch {
      /* Diagnostics channel failure must never affect the statusline */
    }
  }
  dbg({ event: 'apply', workspaceCwd, opts: { ...opts, debugLogPath: undefined } })

  // ── Event-driven updates (user rule: NO polling) ───────────────────────
  // The MCP server rewrites `.dut-serial/inventory.json` ATOMICALLY
  // (tmp+rename) on every state transition. We watch that directory with
  // fs.watch (inotify on Linux): a rename of inventory.json (or its tmp
  // predecessor, which precedes the final rename) triggers an immediate
  // re-render — a state change lands on the statusline within ms, and an
  // idle project costs ZERO CPU (no timers running). The initial render is
  // synchronous; `tui/session-switched` re-targets discovery immediately.
  const dirOf = (project) => join(project, '.dut-serial')

  /** (Re)arm the inventory watcher for the current project. Call whenever
   *  the project may have changed or the previous watcher errored. When
   *  `.dut-serial` does not exist yet (fresh project, no server has ever
   *  written), watch the project root for the directory's creation instead —
   *  the server's FIRST write must not be missed. */
  const armWatcher = () => {
    if (watcher !== null) {
      try {
        watcher.close()
      } catch {
        /* already closed */
      }
      watcher = null
    }
    const project = findProjectDir(workspaceCwd)
    if (project === null) return
    const dir = dirOf(project)
    if (!existsSync(dir)) {
      // Fresh project: watch the project root; the server's first write
      // creates .dut-serial (rename event) → re-arm onto it.
      try {
        watcher = watch(project, (_eventType, filename) => {
          if (filename === '.dut-serial' || String(filename).startsWith('.dut-serial')) {
            dbg({ event: 'fs-watch', filename: String(filename), note: 'dut-serial appeared' })
            armWatcher()
            void tick()
          }
        })
        watcher.on('error', (err) => {
          dbg({ event: 'fs-watch-error', message: err?.message ?? String(err) })
          try {
            watcher?.close()
          } catch {
            /* noop */
          }
          watcher = null
        })
      } catch (err) {
        dbg({ event: 'fs-watch-arm-failed', message: err?.message ?? String(err) })
        watcher = null
      }
      return
    }
    try {
      watcher = watch(dir, (_eventType, filename) => {
        // tmp+rename: the tmp write precedes the final rename — both carry
        // the new state, so either triggers a re-render. A null filename
        // (some platforms) is indistinguishable — re-render conservatively.
        const name = filename === null || filename === undefined ? '' : String(filename)
        if (name === '' || name === INVENTORY_FILE || name.startsWith('.inventory.json.tmp-')) {
          dbg({ event: 'fs-watch', filename: name || null })
          void tick()
        }
      })
      watcher.on('error', (err) => {
        dbg({ event: 'fs-watch-error', message: err?.message ?? String(err) })
        try {
          watcher?.close()
        } catch {
          /* noop */
        }
        watcher = null
      })
    } catch (err) {
      dbg({ event: 'fs-watch-arm-failed', message: err?.message ?? String(err) })
      watcher = null
    }
  }

  const setText = (plain, segments) => {
    // Change detection on both representations (the styled path must not
    // re-emit when only the raw ANSI spelling changed).
    const segKey = segments === undefined ? null : JSON.stringify(segments)
    if (plain === lastText && segKey === lastSegments) return
    lastText = plain
    lastSegments = segKey
    try {
      // Styled segments when the TUI ships setStyled — one capability probe,
      // never a version check. Stock dsh-tui does NOT ship it (contributions
      // render as plain text above the prompt per the official tuiStatus
      // contract), so the plain path is the norm; the probe auto-enables
      // colored runs the day upstream lands the capability.
      latestDispose =
        segments !== undefined && typeof status.setStyled === 'function'
          ? status.setStyled(KEY, segments, ctx)
          : status.set(KEY, plain || undefined, ctx)
    } catch (err) {
      log(`dsh-dut: tuiStatus.set/setStyled failed: ${err?.message ?? err}`)
    }
  }

  const tick = async () => {
    if (ticking) return
    ticking = true
    try {
      let text = ''
      const project = findProjectDir(workspaceCwd)
      dbg({ event: 'tick', workspaceCwd, project })
      if (project) {
        // The project may have just materialized its .dut-serial dir (the
        // auto-start retry, or a session switch into a fresh checkout):
        // arm the watcher so the NEXT server write is still caught.
        if (watcher === null) armWatcher()
        const inv = loadInventory(project)
        const port = mcpPortFor(project, inv)
        let live = false
        if (inv) {
          live = inventoryLive(inv)
          dbg({
            event: 'liveness',
            mcp_pid: inv.mcp_pid ?? null,
            pidAlive: pidAlive(inv.mcp_pid),
            heartbeat: inv.heartbeat_secs ?? null,
            live,
          })
          if (!live && Number.isInteger(inv.mcp_http_port)) {
            live = await portAlive(inv.mcp_http_port, opts.tcpTimeoutMs)
            dbg({ event: 'liveness-tcp', portAlive: live })
          }
        } else {
          // No inventory yet (fresh project): the deterministic port is the
          // only probe target.
          live = await portAlive(port, opts.tcpTimeoutMs)
          dbg({ event: 'liveness-tcp-noninventory', portAlive: live, port })
        }
        if (live) {
          if (inv) {
            text = renderStatus(inv, isGuest(loadOwner(project)))
          }
        } else {
          // Startup dir is a configured project (.target.jsonc found by
          // discovery) but no server answers: bring the MCP up (mirrors the
          // Claude Code session-start hook), rate-limited, and while it is
          // down render the honest disconnected fallback — never a blank
          // line, never a fabricated healthy state.
          if (opts.autoStart && port > 0) {
            const now = Date.now()
            if (now - lastSpawnAt >= opts.autoStartIntervalMs) {
              lastSpawnAt = now
              const result = ensureServer({ projectDir: project, inv, port, config: opts })
              dbg({ event: 'autostart', port, ok: result.ok, reason: result.reason ?? null, pid: result.pid ?? null })
              if (result.ok) {
                if (retryTimer !== null) clearTimeout(retryTimer)
                retryTimer = setTimeout(() => {
                  retryTimer = null
                  // The spawned server creates .dut-serial on first write —
                  // arm the watcher now that the dir exists, then render.
                  armWatcher()
                  void tick()
                }, opts.autoStartRetryMs)
              } else {
                log(`dsh-dut: auto-start failed on :${port} — ${result.reason}`)
              }
            }
          }
          if (inv) text = renderDisconnected(inv)
        }
      }
      const plain = stripAnsi(text)
      const segments =
        typeof status.setStyled === 'function' ? parseAnsiSegments(text) : undefined
      dbg({ event: 'render', plain, segments })
      setText(plain, segments)
    } catch (err) {
      dbg({ event: 'error', message: err?.message ?? String(err) })
      log(`dsh-dut: statusline poll tick failed: ${err?.message ?? err}`)
    } finally {
      ticking = false
    }
  }

  void tick()
  armWatcher()

  // Session switches re-target discovery and the watcher immediately.
  try {
    offSwitch = ctx.on('tui/session-switched', (event) => {
      const cwd = typeof event?.cwd === 'string' && event.cwd !== '' ? event.cwd : null
      dbg({ event: 'session-switched', cwd })
      if (cwd !== null && cwd !== workspaceCwd) {
        workspaceCwd = cwd
        armWatcher()
        void tick()
      }
    })
  } catch (err) {
    log(`dsh-dut: cannot subscribe tui/session-switched: ${err?.message ?? err}`)
  }

  ctx.effect(() => () => {
    if (watcher !== null) {
      try {
        watcher.close()
      } catch {
        /* noop */
      }
      watcher = null
    }
    if (retryTimer !== null) clearTimeout(retryTimer)
    retryTimer = null
    if (typeof offSwitch === 'function') offSwitch()
    if (typeof latestDispose === 'function') latestDispose()
  }, 'dsh-dut statusline watcher')
}

// ── MCP bridge role (project-aware server mode) ────────────────────────────

/**
 * Bridge state machine. One generation per connected server:
 *
 *   gen = { projectDir, port, client, transport, disposers, connected }
 *
 * Rules:
 *   - No project (discovery returns null) → no generation, no tools.
 *   - Project/port change → teardown the old generation (unregister tools,
 *     close transport) and rebuild.
 *   - Server dead → unregister tools (honest: never keep a stale tool that
 *     fails every call), auto-start it (rate-limited), and keep probing.
 *   - Server live, no generation → connect, sync, register.
 *   - Lost transport (onclose) → mark disconnected; the next tick tears down
 *     and reconnects with exponential backoff.
 *
 * @param {object} ctx Cordis context (injects tools, tuiStatus)
 * @param {object} [config] row config: { role: 'mcp', serverName?, ... }
 */
export function applyMcp(ctx, config = {}) {
  const opts = { ...MCP_DEFAULTS, ...config }
  const log = (msg) => {
    try {
      ctx.logger?.debug?.(msg)
    } catch {
      /* Never crash the TUI on a logging failure */
    }
  }

  let gen = null
  let timer = null
  let retryTimer = null
  let offSwitch = null
  let lastSpawnAt = 0
  let ticking = false
  let retryAt = 0
  let retryDelay = opts.reconnectInitialMs
  let workspaceCwd = process.env.DSH_TUI_WORKSPACE_TARGET || process.cwd()

  const dbg = (entry) => {
    if (!opts.debugLogPath) return
    try {
      appendFileSync(
        opts.debugLogPath,
        JSON.stringify({ ts: new Date().toISOString(), role: 'mcp', ...entry }) + '\n',
      )
    } catch {
      /* Diagnostics channel failure must never affect the bridge */
    }
  }
  dbg({ event: 'apply', workspaceCwd, opts: { ...opts, debugLogPath: undefined } })

  const teardown = (target, reason) => {
    if (target === null || target === undefined) return
    dbg({ event: 'teardown', projectDir: target.projectDir, port: target.port, reason })
    for (const dispose of target.disposers?.values() ?? []) {
      try {
        dispose()
      } catch (err) {
        log(`dsh-dut: tool unregister failed: ${err?.message ?? err}`)
      }
    }
    try {
      target.transport?.close?.()
    } catch {
      /* transport close is best-effort */
    }
    try {
      target.client?.close?.()
    } catch {
      /* client close is best-effort */
    }
  }

  /** Connect to 127.0.0.1:<port>/mcp, sync and register tools. Throws on
   *  failure; the caller decides the retry policy. */
  const connect = async (projectDir, port, inv) => {
    const client = new Client({ name: `dsh-dut:${opts.serverName}`, version: '0.1.0' })
    const transport = new StreamableHTTPClientTransport(
      new URL(`http://127.0.0.1:${port}${MCP_PATH}`),
      { requestInit: {} },
    )
    gen = { projectDir, port, client, transport, disposers: new Map(), connected: false }
    try {
      await client.connect(transport)
      gen.connected = true
      gen.disposers = await syncTools(client, ctx, {
        serverName: opts.serverName,
        toolPattern: opts.toolPattern,
        toolCallTimeoutMs: opts.toolCallTimeoutMs,
      })
      retryDelay = opts.reconnectInitialMs
      dbg({ event: 'connected', projectDir, port, tools: gen.disposers.size, serverName: opts.serverName })
      log(`dsh-dut: connected ${opts.serverName} → :${port} (${gen.disposers.size} tools)`)
      transport.onclose = () => {
        dbg({ event: 'transport-close', projectDir, port })
        if (gen !== null && gen.transport === transport) gen.connected = false
      }
    } catch (err) {
      teardown(gen, `connect failed: ${err?.message ?? err}`)
      gen = null
      throw err
    }
  }

  const tick = async () => {
    if (ticking) return
    ticking = true
    try {
      const project = findProjectDir(workspaceCwd)
      dbg({ event: 'tick', workspaceCwd, project, gen: gen === null ? null : { projectDir: gen.projectDir, port: gen.port, connected: gen.connected, tools: gen.disposers.size } })
      if (project === null) {
        // Outside a configured project: never connect, never register.
        if (gen !== null) {
          teardown(gen, 'no project')
          gen = null
          retryDelay = opts.reconnectInitialMs
        }
        return
      }
      const inv = loadInventory(project)
      const port = mcpPortFor(project, inv)
      if (gen !== null && (gen.projectDir !== project || gen.port !== port)) {
        teardown(gen, `project/port changed (${gen.projectDir}:${gen.port} → ${project}:${port})`)
        gen = null
        retryDelay = opts.reconnectInitialMs
      }
      if (gen !== null && gen.connected) {
        // Even while connected, verify the server is still alive: transport
        // onclose is not guaranteed to fire promptly (or at all) when the
        // server process dies, so the inventory pid is the cheap canary and
        // the TCP probe the fallback.
        let pidOk = false
        if (inv) {
          pidOk = pidAlive(inv.mcp_pid)
          dbg({ event: 'liveness-connected', mcp_pid: inv.mcp_pid ?? null, pidAlive: pidOk })
        }
        if (!pidOk && !(await portAlive(port, opts.tcpTimeoutMs))) {
          teardown(gen, 'server unreachable while connected')
          gen = null
          retryDelay = opts.reconnectInitialMs
        }
        return
      }

      // Server liveness: pid first (cheap), TCP probe as fallback.
      let live = false
      if (inv) {
        live = pidAlive(inv.mcp_pid)
        dbg({ event: 'liveness', mcp_pid: inv.mcp_pid ?? null, pidAlive: live, mcp_http_port: inv.mcp_http_port ?? null })
      }
      if (!live) {
        live = await portAlive(port, opts.tcpTimeoutMs)
        dbg({ event: 'liveness-tcp', port, portAlive: live })
      }
      if (!live) {
        // A stale generation means the server died: unregister honestly.
        if (gen !== null) {
          teardown(gen, 'server unreachable')
          gen = null
        }
        if (opts.autoStart && port > 0) {
          const now = Date.now()
          if (now - lastSpawnAt >= opts.autoStartIntervalMs) {
            lastSpawnAt = now
            const result = ensureServer({ projectDir: project, inv, port, config: opts })
            dbg({ event: 'autostart', port, ok: result.ok, reason: result.reason ?? null, pid: result.pid ?? null })
            if (result.ok) {
              if (retryTimer !== null) clearTimeout(retryTimer)
              retryTimer = setTimeout(() => {
                retryTimer = null
                void tick()
              }, opts.autoStartRetryMs)
            } else {
              log(`dsh-dut: auto-start failed on :${port} — ${result.reason}`)
            }
          }
        }
        return
      }
      // Live but not connected: respect the reconnect backoff.
      const now = Date.now()
      if (now < retryAt) return
      try {
        await connect(project, port, inv)
      } catch (err) {
        retryAt = now + retryDelay
        retryDelay = Math.min(retryDelay * 2, opts.reconnectMaxMs)
        dbg({ event: 'connect-failed', port, retryDelay, message: err?.message ?? String(err) })
        log(`dsh-dut: connect :${port} failed — retry in ${retryDelay}ms (${err?.message ?? err})`)
      }
    } catch (err) {
      dbg({ event: 'error', message: err?.message ?? String(err) })
      log(`dsh-dut: mcp bridge tick failed: ${err?.message ?? err}`)
    } finally {
      ticking = false
    }
  }

  void tick()
  timer = setInterval(() => {
    void tick()
  }, opts.pollIntervalMs)

  try {
    offSwitch = ctx.on('tui/session-switched', (event) => {
      const cwd = typeof event?.cwd === 'string' && event.cwd !== '' ? event.cwd : null
      dbg({ event: 'session-switched', cwd })
      if (cwd !== null && cwd !== workspaceCwd) {
        workspaceCwd = cwd
        void tick()
      }
    })
  } catch (err) {
    log(`dsh-dut: cannot subscribe tui/session-switched: ${err?.message ?? err}`)
  }

  ctx.effect(() => () => {
    if (timer !== null) clearInterval(timer)
    timer = null
    if (retryTimer !== null) clearTimeout(retryTimer)
    retryTimer = null
    if (typeof offSwitch === 'function') offSwitch()
    teardown(gen, 'plugin dispose')
    gen = null
  }, 'dsh-dut mcp bridge')
}
