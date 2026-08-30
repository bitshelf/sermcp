/**
 * dsh-dut-statusline — surface sermcp DUT states on the dsh-tui
 * statusline.
 *
 * Data source (identical to the Python hooks, schema_version 1):
 *   MCP Server ──atomic_write──→ <project>/.dut-serial/inventory.json
 *
 * The plugin is EVENT-DRIVEN (fs.watch / inotify on .dut-serial — NO
 * polling): a state change re-renders within ms, an idle project costs
 * zero CPU. A LOW-frequency watchdog (pollIntervalMs, default 20s,
 * unref'd) re-arms a dead watcher, retries discovery, and refreshes the
 * render so stale states age out. On each inventory write it:
 *   1. Project discovery walks up from the session workspace dir
 *      (.dut-serial/inventory.json or .target.jsonc; TARGET_CONF /
 *      TARGET_EXCLUDE semantics match the Python lib). The workspace starts
 *      at DSH_TUI_WORKSPACE_TARGET / process CWD and follows
 *      `tui/session-switched` events (--resume, /workspace switches).
 *   2. Liveness gate (same contract as hooks/claude/inventory.py): the
 *      inventory's mcp_pid must be alive, ACTUALLY be the MCP process
 *      (comm/argv[0] identity), and — when a heartbeat was declared —
 *      fresh (written_at ≤ 3 × heartbeat_secs). Dead pid or stale
 *      heartbeat → the explicit `✗ <dut>:disconnected` placeholder,
 *      NEVER the stale snapshot: a TCP probe must not gate liveness (it
 *      can only annotate "port still held by a zombie"). A dead server
 *      inside a configured project triggers AUTO-START: the plugin spawns
 *      the project's MCP server (the startup dir has .target.jsonc →
 *      bring the server up, exactly like the Claude Code session-start
 *      hook), rate-limited, and while the server is down the contribution
 *      honestly reads `✗ <dut>:disconnected` — never a blank line, never
 *      a made-up healthy state.
 *   3. Per-DUT `state_text` (the Rust pre-formatted ANSI string, e.g.
 *      `\x1b[32m● <dut>:active\x1b[0m` — same verbatim source as
 *      the Claude Code statusline hook) is joined with " · " and handed to
 *      the TUI via `ctx.tuiStatus`. The plugin renders TWO views of the
 *      same string:
 *        - `tuiStatus.setStyled(key, segments)` when the TUI supports it
 *          (dsh-tui ≥ 0.9.4): the ANSI is parsed into `{ text, fg }` runs
 *          and the TUI renders them colored, rightmost in the statusline
 *          row — no TUI patch, upstream capability;
 *        - plain `tuiStatus.set(key, text)` otherwise: the ANSI is stripped
 *          and the contribution renders in whatever home older TUIs give
 *          plain status text (dim, no colors). Capability detection is
 *          `typeof ctx.tuiStatus.setStyled === 'function'` — one probe,
 *          never a version string. The state→color mapping still lives
 *          only in Rust; the plugin only re-encodes the ANSI it receives.
 *
 * Contracts:
 *   - No TUI-internal imports, no pnpm patch, no dsh-tui version pinning:
 *     only the stable `tuiStatus` seam and the `tui/session-switched` event
 *     (peerDep is for types only). dsh-tui upgrades need zero rework.
 *   - The statusline never throws: all I/O, spawn and set() calls are fenced
 *     and failures only produce debug logs.
 *   - Each tick is one small JSON read plus at most one local TCP connect;
 *     nothing is written. Auto-start spawns at most once per
 *     `autoStartIntervalMs` (default 60s) and only when no server answers.
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
import { basename, delimiter, dirname, isAbsolute, join, resolve, sep } from 'node:path'
import net from 'node:net'

export const name = 'dut-statusline'
/** Code-level dependency: compositions without the tuiStatus service fail
 *  loudly at load instead of silently doing nothing. */
export const inject = ['tuiStatus']

export const DEFAULTS = Object.freeze({
  /** Watchdog interval (ms, default 20s) — a LOW-frequency safety net on
   *  top of the event-driven fs.watch updates, NOT a render poll. Each
   *  fire: re-arms a watcher that died (error paths null it), retries
   *  project discovery when the first tick found no project, and
   *  refreshes the render once so stale states age out via the freshness
   *  gate. Honored as the old pollIntervalMs option so existing configs
   *  keep working (the old 2s poll default would be far too hot). */
  pollIntervalMs: 20000,
  /** Timeout (ms) of the TCP probe used to annotate "port still held by a
   *  zombie" while the server is down (never gates liveness) and to
   *  suppress duplicate auto-starts on fresh projects. */
  tcpTimeoutMs: 300,
  /** Auto-start the project's MCP server when no server answers and the
   *  startup dir is a configured project (.target.jsonc). Mirrors the Claude
   *  Code session-start hook; `false` disables (then a dead server renders
   *  the disconnected fallback without spawning). */
  autoStart: true,
  /** Minimum gap between auto-start spawn attempts (ms). */
  autoStartIntervalMs: 60000,
  /** Delay before re-polling after a successful auto-start spawn (ms) —
   *  the server needs a moment to bind and rewrite inventory.json. */
  autoStartRetryMs: 3000,
  /** Explicit path to the sermcp binary; default: PATH lookup
   *  then the usual install dirs. */
  serverBinary: undefined,
  /** Debug: append per-tick decisions as JSON lines to this path;
   *  DUT_STATUSLINE_DEBUG env takes precedence. Diagnostics only. */
  debugLogPath: process.env.DUT_STATUSLINE_DEBUG ?? undefined,
})

const INVENTORY_FILE = 'inventory.json'
const CONFIG_FILE = '.target.jsonc'
/** Directories never treated as project roots themselves (matches
 *  hooks/claude/inventory.py). */
const FORBIDDEN_ROOTS = new Set(['/tmp', '/var/tmp', '/dev/shm', '/run', '/proc', '/sys', '/dev'])
/** tuiStatus key: lowercase slug, plugin-namespaced. */
const KEY = 'dut-statusline'

// ── Project discovery (semantics aligned with the Python lib) ──────────────

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
export function portAlive(port, timeoutMs = DEFAULTS.tcpTimeoutMs) {
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

// ── Rendering ──────────────────────────────────────────────────────────────

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

// ── MCP server auto-start (startup dir has .target.jsonc → bring it up) ───
//
// The port formulas below are the JS mirror of `sermcp/src/ports.rs`
// (`project_mcp_port` / `project_dut_mcp_port`), byte-identical to the
// session-start hook. The selftest pins known-answer vectors for the
// formula (neutral path + DUT names, machine-independent).

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

// ── Plugin body ────────────────────────────────────────────────────────────

/**
 * @param {object} ctx Cordis context (injects tuiStatus)
 * @param {object} [config] row config: { pollIntervalMs?, tcpTimeoutMs?, ... }
 */
export function apply(ctx, config = {}) {
  const opts = { ...DEFAULTS, ...config }
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
  let watchdogTimer = null
  let lastSpawnAt = 0
  let ticking = false
  let pendingTick = false
  let lastText = '\u0000' // sentinel: force the first write (including a first clear)
  let lastSegments = null
  // Watchdog health accounting: consecutive arm/error failures. Past the
  // cap the plugin shows a visible "degraded" note instead of silently
  // dying; the note UNLATCHES when a later arm succeeds (the watchdog
  // keeps retrying, so recovery is the expected outcome).
  let watcherFailures = 0
  let degradedNoted = false
  const WATCHER_FAILURE_CAP = 3
  const DEGRADED_NOTE = '⚠ dut-statusline degraded (fs.watch failing — watchdog recovering)'
  // Session workspace: launch override > process CWD; then `tui/session-switched`
  // (--resume, /workspace switches, rewind) takes over so we never stare at
  // the launch CWD forever.
  let workspaceCwd = process.env.DSH_TUI_WORKSPACE_TARGET || process.cwd()

  // Test seam: the selftest injects a controllable watch() through config.
  const doWatch = typeof config.watchImpl === 'function' ? config.watchImpl : watch

  const dbg = (entry) => {
    if (!opts.debugLogPath) return
    try {
      appendFileSync(
        opts.debugLogPath,
        JSON.stringify({ ts: new Date().toISOString(), ...entry }) + '\n',
      )
    } catch {
      /* Diagnostics channel failure must never affect the statusline */
    }
  }
  dbg({ event: 'apply', workspaceCwd, opts: { ...opts, debugLogPath: undefined } })

  // ── Event-driven updates (no polling; see the watchdog below) ──────────
  // The MCP server rewrites `.dut-serial/inventory.json` ATOMICALLY
  // (tmp+rename) on every state transition. We watch that directory with
  // fs.watch (inotify on Linux): a rename of inventory.json (or its tmp
  // predecessor, which precedes the final rename) triggers an immediate
  // re-render — a state change lands on the statusline within ms, and an
  // idle project costs ZERO CPU (no render timers). The initial render is
  // synchronous; `tui/session-switched` re-targets discovery immediately.
  const dirOf = (project) => join(project, '.dut-serial')

  const countWatchFailure = () => {
    watcherFailures += 1
    dbg({ event: 'watch-failure-count', watcherFailures })
    if (watcherFailures >= WATCHER_FAILURE_CAP && !degradedNoted) {
      degradedNoted = true
      log(`dut-statusline: ${DEGRADED_NOTE}`)
      // Visible: render the note instead of silently going blank.
      setText(DEGRADED_NOTE, undefined)
    }
  }

  /** (Re)arm the inventory watcher for the current project. Call whenever
   *  the project may have changed or the previous watcher errored. When
   *  `.dut-serial` does not exist yet (fresh project, no server has ever
   *  written), watch the project root for the directory's creation instead —
   *  the server's FIRST write must not be missed.
   *  Returns true when a watcher is armed after the call. The error
   *  handlers null `watcher` (and count the failure) — the WATCHDOG below
   *  re-arms on its next fire, so a transient fs.watch failure never
   *  becomes a permanently dead statusline. */
  /** Successful arm: failure count resets and a latched "degraded" note
   *  un-latches (recovery is the watchdog's expected outcome). */
  const armResult = () => {
    if (watcher === null) return false
    watcherFailures = 0
    if (degradedNoted) {
      degradedNoted = false
      log('dut-statusline: fs.watch recovered — degraded note cleared')
    }
    return true
  }

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
    if (project === null) return false
    const dir = dirOf(project)
    if (!existsSync(dir)) {
      // Fresh project: watch the project root; the server's first write
      // creates .dut-serial (rename event) → re-arm onto it.
      try {
        watcher = doWatch(project, (_eventType, filename) => {
          if (filename === '.dut-serial' || String(filename).startsWith('.dut-serial')) {
            dbg({ event: 'fs-watch', filename: String(filename), note: 'dut-serial appeared' })
            armWatcher()
            void tick()
          }
        })
        attachErrorHandler()
      } catch (err) {
        dbg({ event: 'fs-watch-arm-failed', message: err?.message ?? String(err) })
        watcher = null
        countWatchFailure()
        return false
      }
      return armResult()
    }
    try {
      watcher = doWatch(dir, (_eventType, filename) => {
        // tmp+rename: the tmp write precedes the final rename — both carry
        // the new state, so either triggers a re-render. A null filename
        // (some platforms) is indistinguishable — re-render conservatively.
        const name = filename === null || filename === undefined ? '' : String(filename)
        if (name === '' || name === INVENTORY_FILE || name.startsWith('.inventory.json.tmp-')) {
          dbg({ event: 'fs-watch', filename: name || null })
          void tick()
        }
      })
      attachErrorHandler()
    } catch (err) {
      dbg({ event: 'fs-watch-arm-failed', message: err?.message ?? String(err) })
      watcher = null
      countWatchFailure()
      return false
    }
    return armResult()
  }

  /** Shared fs.watch error handler: close, null the watcher (the watchdog
   *  re-arms next cycle), and count the failure toward the degraded cap. */
  function attachErrorHandler() {
    if (watcher === null) return
    watcher.on('error', (err) => {
      dbg({ event: 'fs-watch-error', message: err?.message ?? String(err) })
      try {
        watcher?.close()
      } catch {
        /* noop */
      }
      watcher = null
      countWatchFailure()
    })
  }

  const setText = (plain, segments) => {
    // Change detection on both representations (the styled path must not
    // re-emit when only the raw ANSI spelling changed).
    const segKey = segments === undefined ? null : JSON.stringify(segments)
    if (plain === lastText && segKey === lastSegments) return
    lastText = plain
    lastSegments = segKey
    try {
      // identity = the plugin's own ctx: the effect ledger records the real
      // pluginId. The returned disposer is already bound to this fiber by
      // the runtime; keep a copy as belt-and-braces.
      // Styled segments when the TUI ships setStyled (dsh-tui ≥ 0.9.4),
      // plain text otherwise — one capability probe, never a version check.
      latestDispose =
        segments !== undefined && typeof status.setStyled === 'function'
          ? status.setStyled(KEY, segments, ctx)
          : status.set(KEY, plain || undefined, ctx)
    } catch (err) {
      log(`dut-statusline: tuiStatus.set/setStyled failed: ${err?.message ?? err}`)
    }
  }

  const tick = async () => {
    if (ticking) {
      // Coalesce, never drop: a burst of fs.watch events (state transition +
      // tmp+rename pairs) must end in ONE trailing render built from the
      // post-burst inventory — a dropped tick can leave a stale frame that
      // no further event corrects.
      pendingTick = true
      return
    }
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
        let portHeld = false
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
            // Annotation ONLY (header contract): a TCP answer cannot
            // resurrect a failed identity/freshness gate — ANY process can
            // hold the recorded port after the server died. Flipping `live`
            // here would render the dead server's stale snapshot as healthy
            // and suppress the auto-start forever.
            portHeld = await portAlive(inv.mcp_http_port, opts.tcpTimeoutMs)
            dbg({ event: 'liveness-tcp', portHeld })
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
                log(`dut-statusline: auto-start failed on :${port} — ${result.reason}`)
              }
            }
          }
          if (inv) {
            text = renderDisconnected(inv)
            if (portHeld) {
              text += ' (port held)'
              dbg({ event: 'annotate-port-held' })
            }
          }
        }
      }
      const plain = stripAnsi(text)
      const segments =
        typeof status.setStyled === 'function' ? parseAnsiSegments(text) : undefined
      dbg({ event: 'render', plain, segments })
      setText(plain, segments)
    } catch (err) {
      dbg({ event: 'error', message: err?.message ?? String(err) })
      log(`dut-statusline: poll tick failed: ${err?.message ?? err}`)
    } finally {
      ticking = false
      if (pendingTick) {
        pendingTick = false
        void tick()
      }
    }
  }

  void tick()
  armWatcher()

  // The watchdog the header promises: a LOW-frequency safety net covering
  // exactly what the event stream cannot — a watcher killed by an error,
  // a project dir that materialized without a watched event, and the
  // render refresh that lets a dead server's snapshot age out through the
  // freshness gate (a kill -9/OOM death produces NO further event; without
  // this timer the last rendered frame would freeze forever). Unref'd: it
  // must never keep the process alive on its own.
  watchdogTimer = setInterval(() => {
    armWatcher()
    void tick()
  }, opts.pollIntervalMs)
  watchdogTimer.unref?.()

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
    log(`dut-statusline: cannot subscribe tui/session-switched: ${err?.message ?? err}`)
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
    if (watchdogTimer !== null) clearInterval(watchdogTimer)
    watchdogTimer = null
    if (typeof offSwitch === 'function') offSwitch()
    if (typeof latestDispose === 'function') latestDispose()
  }, 'dut-statusline watcher')
}
