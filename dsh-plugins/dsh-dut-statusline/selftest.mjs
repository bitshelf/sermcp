#!/usr/bin/env node
/**
 * dsh-dut-statusline selftest — same style as the Python hooks' --selftest.
 *
 * Run: node dsh-plugins/dsh-dut-statusline/selftest.mjs
 * Covers:
 *   - renderStatus / renderDisconnected: state_text (ANSI) joined verbatim,
 *     no current-dut marker, empty inputs render empty;
 *   - parseAnsiSegments / stripAnsi: the setStyled capability path and the
 *     plain-text fallback derived from the same ANSI string;
 *   - findProjectDir: TARGET_CONF override, walk-up, TARGET_EXCLUDE, forbidden roots;
 *   - loadInventory: missing / corrupt / wrong version → null;
 *   - port mirroring (projectPort / dutPort) vs the ports.rs vectors;
 *   - Liveness contract: identity/freshness gates decide liveness; a live
 *     TCP port is ANNOTATION ONLY (mcp_pid null + live port renders the
 *     disconnected fallback with a "(port held)" note, never the stale
 *     snapshot); dead port renders the disconnected fallback;
 *   - Watchdog: fires on pollIntervalMs and re-arms the watcher (a dead
 *     watcher must not freeze the statusline forever);
 *   - Integration (mock ctx): live pid renders immediately; state change →
 *     fs.watch re-render; session switch re-targets; auto-start rate limit;
 *     unload → cleanup; ctx WITH setStyled renders segments, ctx WITHOUT
 *     falls back to plain text.
 */
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { spawn } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join, resolve, sep } from 'node:path'
import net from 'node:net'

import {
  DEFAULTS,
  apply,
  canonicalDir,
  dutPort,
  ensureServer,
  findProjectDir,
  isGuest,
  loadInventory,
  loadOwner,
  mcpPortFor,
  ownerIsAlive,
  parseAnsiSegments,
  inventoryLive,
  pidAlive,
  pidIsMcpServer,
  portAlive,
  projectPort,
  renderDisconnected,
  renderStatus,
  resolveServerBinary,
  serverCommand,
  serverEnv,
  stripAnsi,
} from './index.js'

let failed = 0
const check = (label, cond, detail = '') => {
  if (cond) console.log(`OK    ${label}`)
  else {
    console.log(`FAIL  ${label}${detail ? ` — ${detail}` : ''}`)
    failed += 1
  }
}

// ── Pure functions ───────────────────────────────────────────────────────

{
  // state_text carries Rust's ANSI colors — the same verbatim source the
  // Claude Code statusline hook renders.
  const activeText = '\x1b[32m● alpha:active\x1b[0m'
  const crashedText = '\x1b[31m✗ beta:crashed\x1b[0m'
  const inv = {
    current_dut: 'alpha',
    duts: [
      { dut_name: 'alpha', state_text: activeText },
      { dut_name: 'beta', state_text: crashedText },
      { dut_name: 'ghost', state_text: '' },
    ],
  }
  check(
    'renderStatus joins non-empty state_text verbatim (no current-dut marker)',
    renderStatus(inv) === `${activeText} · ${crashedText}`,
    JSON.stringify(renderStatus(inv)),
  )
  check('renderStatus without duts renders empty', renderStatus({ duts: [] }) === '')
  check('renderStatus without inventory renders empty', renderStatus(null) === '')
}

// ── ANSI → structured segments (the setStyled capability path) ──────────────

{
  check(
    'parseAnsiSegments maps SGR colors to the segment palette',
    JSON.stringify(parseAnsiSegments('\x1b[32m● a:active\x1b[0m')) ===
      JSON.stringify([{ text: '● a:active', fg: 'ansi:green' }]),
    JSON.stringify(parseAnsiSegments('\x1b[32m● a:active\x1b[0m')),
  )
  check(
    'parseAnsiSegments keeps multi-run color resets',
    JSON.stringify(parseAnsiSegments('\x1b[32m● a:active\x1b[0m · \x1b[31m✗ b:crashed\x1b[0m')) ===
      JSON.stringify([
        { text: '● a:active', fg: 'ansi:green' },
        { text: ' · ' },
        { text: '✗ b:crashed', fg: 'ansi:red' },
      ]),
    JSON.stringify(parseAnsiSegments('\x1b[32m● a:active\x1b[0m · \x1b[31m✗ b:crashed\x1b[0m')),
  )
  check(
    'parseAnsiSegments bright codes map to the bright palette',
    parseAnsiSegments('\x1b[91m✗ a\x1b[0m')[0]?.fg === 'ansi:redBright',
  )
  check('parseAnsiSegments empty → []', JSON.stringify(parseAnsiSegments('')) === '[]')
  check(
    'stripAnsi removes SGR codes (the old-TUI fallback)',
    stripAnsi('\x1b[32m● a:active\x1b[0m') === '● a:active',
  )
}

// ── Disconnected fallback ──────────────────────────────────────────────────

{
  check(
    'renderDisconnected joins dut names as red ✗ name:disconnected',
    renderDisconnected({ duts: [{ dut_name: 'a' }, { dut_name: 'b', state_plain: 'x' }, { dut_name: '' }] }) ===
      '\x1b[31m✗ a:disconnected\x1b[0m · \x1b[31m✗ b:disconnected\x1b[0m',
    JSON.stringify(renderDisconnected({ duts: [{ dut_name: 'a' }, { dut_name: 'b' }] })),
  )
  check('renderDisconnected without duts renders empty', renderDisconnected({ duts: [] }) === '')
  check('renderDisconnected null renders empty', renderDisconnected(null) === '')
}

// ── Port mirror (must stay byte-identical to sermcp/src/ports.rs) ──────────
// Known-answer vectors on a NEUTRAL path and DUT names — the md5-based port
// formula is deterministic on every machine, so the expected values are
// fixed numbers, not this machine's project layout.

{
  check(
    'projectPort matches ports.rs mirror (/work/dut-project → 3003)',
    projectPort('/work/dut-project') === 3003,
    String(projectPort('/work/dut-project')),
  )
  check(
    'dutPort matches ports.rs mirror (/work/dut-project:alpha → 3098)',
    dutPort('/work/dut-project', 'alpha') === 3098,
    String(dutPort('/work/dut-project', 'alpha')),
  )
  check(
    'mcpPortFor prefers the recorded inventory port',
    mcpPortFor('/work/dut-project', { mcp_http_port: 3017 }) === 3017,
  )
  check(
    'mcpPortFor falls back to the current_dut port',
    mcpPortFor('/work/dut-project', { current_dut: 'alpha' }) === 3098,
  )
  check(
    'mcpPortFor falls back to the first dut port',
    mcpPortFor('/work/dut-project', { duts: [{ dut_name: 'alpha' }] }) === 3098,
  )
  check(
    'mcpPortFor without inventory falls back to the project port',
    mcpPortFor('/work/dut-project', null) === 3003,
  )
  check('canonicalDir resolves symlinks', canonicalDir('/work/dut-project') === '/work/dut-project')
}

// ── Auto-start helpers (binary lookup + spawn shape) ───────────────────────

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-bin-'))
  try {
    const fakeBin = join(tmp, 'fake-sermcp')
    writeFileSync(fakeBin, '')
    check(
      'resolveServerBinary honors an explicit existing override',
      resolveServerBinary(fakeBin) === fakeBin,
    )
    check(
      'resolveServerBinary rejects a missing override',
      resolveServerBinary(join(tmp, 'nope')) === null,
    )

    let spawned = null
    const stubSpawn = (bin, args, opts) => {
      spawned = { bin, args, opts }
      return { pid: 424242, unref() {} }
    }
    const inv = { mcp_http_port: 3017, current_dut: 'alpha', duts: [{ dut_name: 'alpha' }] }
    const result = ensureServer({
      projectDir: tmp,
      inv,
      port: 3017,
      config: { serverBinary: fakeBin },
      spawnImpl: stubSpawn,
    })
    check(
      'ensureServer spawns `--http 127.0.0.1:<port>`',
      result.ok && spawned?.args?.[0] === '--http' && spawned.args[1] === '127.0.0.1:3017',
      JSON.stringify(spawned?.args),
    )
    check(
      'ensureServer runs detached with cwd = project dir',
      result.ok && spawned?.opts?.detached === true && spawned.opts.cwd === tmp,
    )
    check(
      'ensureServer env carries TARGET_CONF and TARGET_DUT_NAME',
      result.ok &&
        spawned.opts.env.TARGET_CONF === join(tmp, '.target.jsonc') &&
        spawned.opts.env.TARGET_DUT_NAME === 'alpha',
    )
    check(
      'ensureServer with missing binary → ok:false',
      !ensureServer({ projectDir: tmp, inv, port: 3017, config: { serverBinary: join(tmp, 'missing') }, spawnImpl: stubSpawn }).ok,
    )
    check('serverCommand shape', serverCommand(3017).join(' ') === '--http 127.0.0.1:3017')
    check(
      'serverEnv without inventory keeps TARGET_CONF only',
      serverEnv(tmp, null).TARGET_CONF === join(tmp, '.target.jsonc') &&
        serverEnv(tmp, null).TARGET_DUT_NAME === undefined,
    )
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-proj-'))
  try {
    mkdirSync(join(tmp, '.dut-serial'), { recursive: true })
    const inventory = join(tmp, '.dut-serial', 'inventory.json')
    writeFileSync(inventory, JSON.stringify({ schema_version: 1, duts: [] }))

    check('findProjectDir walks up to inventory.json', findProjectDir(join(tmp, 'a', 'b')) === resolve(tmp))

    writeFileSync(join(tmp, '.target.jsonc'), '')
    const oldTargetConf = process.env.TARGET_CONF
    process.env.TARGET_CONF = join(tmp, '.target.jsonc')
    check('findProjectDir honors TARGET_CONF override', findProjectDir('/somewhere/else') === resolve(tmp))
    process.env.TARGET_CONF = join(tmp, 'missing.jsonc')
    check('findProjectDir with missing TARGET_CONF → null', findProjectDir() === null)
    if (oldTargetConf === undefined) delete process.env.TARGET_CONF
    else process.env.TARGET_CONF = oldTargetConf

    const oldExclude = process.env.TARGET_EXCLUDE
    process.env.TARGET_EXCLUDE = resolve(tmp) + ':/other/place'
    check('findProjectDir honors TARGET_EXCLUDE', findProjectDir(tmp) === null)
    if (oldExclude === undefined) delete process.env.TARGET_EXCLUDE
    else process.env.TARGET_EXCLUDE = oldExclude

    check(
      'findProjectDir rejects forbidden roots',
      findProjectDir(join('/tmp', 'x')) === null || findProjectDir('/tmp') === null,
    )

    check('loadInventory reads valid schema', loadInventory(tmp)?.duts !== undefined)
    writeFileSync(inventory, '{ broken')
    check('loadInventory rejects malformed JSON', loadInventory(tmp) === null)
    writeFileSync(inventory, JSON.stringify({ schema_version: 2 }))
    check('loadInventory rejects wrong schema_version', loadInventory(tmp) === null)
    rmSync(inventory)
    check('loadInventory missing file → null', loadInventory(tmp) === null)
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  check('pidAlive accepts the live test pid', pidAlive(process.pid))
  check('pidAlive rejects null/zero', !pidAlive(null) && !pidAlive(0) && !pidAlive(-1))
  check('pidAlive rejects dead pid', !pidAlive(1 << 30))
}

// ── Ownership (multi-agent isolation) ──────────────────────────────────────

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-owner-'))
  try {
    const project = join(tmp, 'proj')
    mkdirSync(join(project, '.dut-serial'), { recursive: true })
    const ownerPath = join(project, '.dut-serial', 'owner.json')
    const writeOwner = (obj) => writeFileSync(ownerPath, JSON.stringify(obj))

    check('loadOwner missing → null', loadOwner(project) === null)
    writeOwner({ schema_version: 2, owner_mcp_pid: process.pid })
    check('loadOwner rejects wrong schema_version', loadOwner(project) === null)

    writeOwner({ schema_version: 1, owner_mcp_pid: process.pid, owner_pid: 424242, claimed_at_unix: Math.floor(Date.now() / 1000) })
    check('ownerIsAlive with live anchor pid', ownerIsAlive(loadOwner(project)))
    check('isGuest when owner pid is not ours', isGuest(loadOwner(project)) === true)
    check('isGuest when owner pid IS ours', isGuest(loadOwner(project), 424242) === false)

    writeOwner({ schema_version: 1, owner_mcp_pid: 1 << 30, owner_pid: 424242, claimed_at_unix: Math.floor(Date.now() / 1000) })
    check('ownerIsAlive false with dead anchor (no grace — anchor present)', !ownerIsAlive(loadOwner(project)))
    check('isGuest empty when owner dead', isGuest(loadOwner(project)) === false)

    // Grace window: anchor not yet back-filled by the server.
    writeOwner({ schema_version: 1, owner_mcp_pid: null, owner_pid: 424242, claimed_at_unix: Math.floor(Date.now() / 1000) })
    check('ownerIsAlive honors claim grace', ownerIsAlive(loadOwner(project)))
    writeOwner({ schema_version: 1, owner_mcp_pid: null, owner_pid: 424242, claimed_at_unix: Math.floor(Date.now() / 1000) - 120 })
    check('ownerIsAlive false after grace expires', !ownerIsAlive(loadOwner(project)))
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

// ── Integration: mock ctx ─────────────────────────────────────────────────

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

function makeMockCtx({ styled = false } = {}) {
  const calls = []
  const handlers = new Map()
  let teardown = null
  const disposers = []
  const tuiStatus = {
    set(key, text, identity) {
      calls.push({ key, text, identity })
      const dispose = () => disposers.push('disposed')
      return dispose
    },
  }
  if (styled) {
    // dsh-tui ≥ 0.9.4 seam: styled contributions land in the statusline row.
    tuiStatus.setStyled = (key, segments, identity) => {
      calls.push({ key, segments, identity })
      const dispose = () => disposers.push('disposed')
      return dispose
    }
  }
  const ctx = {
    tuiStatus,
    logger: { debug: () => {} },
    on(name, handler) {
      handlers.set(name, handler)
      disposers.push(`off:${name}`)
      return () => disposers.push(`offed:${name}`)
    },
    effect(fn) {
      teardown = fn()
      return teardown
    },
  }
  return { ctx, calls, getTeardown: () => teardown, disposers, handlers }
}

function writeInventory(projectDir, inv) {
  const dir = join(projectDir, '.dut-serial')
  mkdirSync(dir, { recursive: true })
  // Atomic tmp+rename — exactly how the MCP server writes: the rename is
  // what the fs.watch (inotify) event-driven statusline reacts to.
  const tmp = join(dir, `.inventory.json.tmp-${process.pid}`)
  writeFileSync(tmp, JSON.stringify(inv))
  renameSync(tmp, join(dir, 'inventory.json'))
}

/** Spawn a long-lived process whose argv[0] masquerades as the MCP binary
 *  (`exec -a sermcp sleep 60`), so pidIsMcpServer/inventoryLive
 *  accept it as the "live server" in the identity-gate tests. Waits until
 *  the bash wrapper has exec'd (argv0 becomes sermcp) so the
 *  returned pid is immediately recognizable. Returns { pid, kill }. */
async function spawnFakeMcp() {
  const fake = spawn('bash', ['-c', 'exec -a sermcp sleep 60'], {
    stdio: 'ignore',
  })
  // Poll /proc/<pid>/cmdline until the exec lands (usually <10ms).
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    try {
      const argv0 = readFileSync(`/proc/${fake.pid}/cmdline`, 'utf8').split('\0')[0]
      if (argv0 === 'sermcp') break
    } catch {
      /* pid not ready */
    }
    await sleep(10)
  }
  return { pid: fake.pid, kill: () => { try { fake.kill('SIGKILL') } catch { /* noop */ } } }
}

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-live-'))
  const fake = await spawnFakeMcp()
  try {
    const project = join(tmp, 'proj')
    writeInventory(project, {
      schema_version: 1,
      written_by: 'mcp-server',
      mcp_pid: fake.pid,
      heartbeat_secs: 30,
      written_at_unix: Math.floor(Date.now() / 1000),
      mcp_http_port: null,
      current_dut: null,
      duts: [
        { dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' },
        { dut_name: 'ghost', state: 'unknown', state_plain: '' },
      ],
    })
    const { ctx, calls, getTeardown, disposers } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, { tcpTimeoutMs: 100, autoStart: false })
      check(
        'live pid renders immediately (first render synchronous, no setStyled → ANSI-stripped plain text)',
        calls.length === 1 && calls[0].key === 'dut-statusline' && calls[0].text === '● alpha:active',
        JSON.stringify(calls),
      )
      check('identity passed as plugin ctx', calls[0].identity === ctx)

      // state change → re-rendered by the fs.watch event (no polling)
      writeInventory(project, {
        schema_version: 1,
        written_by: 'mcp-server',
        mcp_pid: fake.pid,
        heartbeat_secs: 30,
        written_at_unix: Math.floor(Date.now() / 1000),
        duts: [{ dut_name: 'alpha', state: 'crashed', state_text: '\x1b[31m✗ alpha:crashed\x1b[0m' }],
      })
      await sleep(150)
      check(
        'state change re-renders on the fs.watch event (no polling)',
        calls.at(-1).text === '✗ alpha:crashed',
        JSON.stringify(calls.at(-1)),
      )

      // server dead → honest disconnected fallback (never stale healthy
      // state, never a blank line inside a configured project)
      writeInventory(project, {
        schema_version: 1,
        written_by: 'mcp-server',
        mcp_pid: 1 << 30,
        mcp_http_port: null,
        duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
      })
      await sleep(120)
      check(
        'dead server renders the disconnected fallback',
        calls.at(-1).text === '✗ alpha:disconnected',
        JSON.stringify(calls.at(-1)),
      )

      // Identity gate: a live pid that is NOT an MCP process must not render.
      writeInventory(project, {
        schema_version: 1,
        written_by: 'mcp-server',
        mcp_pid: process.pid,
        heartbeat_secs: 30,
        written_at_unix: Math.floor(Date.now() / 1000),
        duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
      })
      await sleep(150)
      check(
        'recycled/foreign pid does NOT render (identity gate)',
        calls.at(-1).text === '✗ alpha:disconnected',
        JSON.stringify(calls.at(-1)),
      )
      check('pidIsMcpServer rejects the node process', !pidIsMcpServer(process.pid))
      check('pidIsMcpServer accepts the fake MCP argv0', pidIsMcpServer(fake.pid))

      // Freshness gate: live MCP pid but STALE written_at → not live.
      writeInventory(project, {
        schema_version: 1,
        written_by: 'mcp-server',
        mcp_pid: fake.pid,
        heartbeat_secs: 30,
        written_at_unix: Math.floor(Date.now() / 1000) - 3600,
        duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
      })
      await sleep(150)
      check(
        'live pid + stale heartbeat snapshot does NOT render (freshness gate)',
        calls.at(-1).text === '✗ alpha:disconnected',
        JSON.stringify(calls.at(-1)),
      )
      check(
        'inventoryLive: fresh heartbeat passes, stale fails',
        inventoryLive({
          mcp_pid: fake.pid,
          heartbeat_secs: 30,
          written_at_unix: Math.floor(Date.now() / 1000),
        }) === true &&
          inventoryLive({
            mcp_pid: fake.pid,
            heartbeat_secs: 30,
            written_at_unix: Math.floor(Date.now() / 1000) - 3600,
          }) === false &&
          inventoryLive({ mcp_pid: fake.pid }) === true, // no heartbeat → identity gate only
      )

      // unload → watcher and disposer cleanup
      const before = calls.length
      getTeardown()()
      await sleep(120)
      check('unload closes the watcher and disposes', calls.length === before && disposers.length >= 1)
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    fake.kill()
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  // ctx WITH setStyled (dsh-tui ≥ 0.9.4): the contribution carries
  // structured segments — the ANSI source is parsed, never passed through.
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-styled-'))
  const fake = await spawnFakeMcp()
  try {
    const project = join(tmp, 'proj')
    writeInventory(project, {
      schema_version: 1,
      written_by: 'mcp-server',
      mcp_pid: fake.pid,
      heartbeat_secs: 30,
      written_at_unix: Math.floor(Date.now() / 1000),
      current_dut: 'alpha',
      duts: [
        { dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' },
        { dut_name: 'beta', state: 'crashed', state_text: '\x1b[31m✗ beta:crashed\x1b[0m' },
      ],
    })
    const { ctx, calls, getTeardown } = makeMockCtx({ styled: true })
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, { autoStart: false })
      check(
        'setStyled receives parsed segments with colors',
        calls.length === 1 &&
          calls[0].key === 'dut-statusline' &&
          JSON.stringify(calls[0].segments) ===
            JSON.stringify([
              { text: '● alpha:active', fg: 'ansi:green' },
              { text: ' · ' },
              { text: '✗ beta:crashed', fg: 'ansi:red' },
            ]),
        JSON.stringify(calls[0]),
      )
      check('styled identity passed as plugin ctx', calls[0].identity === ctx)
      getTeardown()()
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    fake.kill()
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  // auto-start: dead server + configured project → spawns the MCP server
  // (rate-limited); spawn failure still renders the disconnected fallback.
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-autostart-'))
  try {
    const project = join(tmp, 'proj')
    const fakeBin = join(tmp, 'fake-sermcp')
    writeFileSync(fakeBin, '')
    let spawnCount = 0
    const lastSpawns = []
    const stubSpawn = (bin, args, opts) => {
      spawnCount += 1
      lastSpawns.push({ bin, args, opts })
      return { pid: 999, unref() {} }
    }
    writeInventory(project, {
      schema_version: 1,
      written_by: 'mcp-server',
      mcp_pid: 1 << 30,
      mcp_http_port: null,
      duts: [{ dut_name: 'a', state: 'active', state_text: '\x1b[32m● a:active\x1b[0m' }],
    })
    const { ctx, calls, getTeardown } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, {
        tcpTimeoutMs: 100,
        autoStart: true,
        autoStartIntervalMs: 5000,
        autoStartRetryMs: 200,
        serverBinary: fakeBin,
        spawnImpl: stubSpawn,
      })
      await sleep(180)
      check(
        'auto-start spawns the MCP server when the gate fails',
        spawnCount === 1 && lastSpawns[0]?.args?.[1] === `127.0.0.1:${dutPort(project, 'a')}`,
        JSON.stringify(lastSpawns),
      )
      check(
        'dead server renders the disconnected fallback while down',
        calls.at(-1).text === '✗ a:disconnected',
        JSON.stringify(calls.at(-1)),
      )
      await sleep(150)
      check(
        'auto-start is rate-limited within autoStartIntervalMs',
        spawnCount === 1,
        `spawned ${spawnCount} times`,
      )
      getTeardown()()
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-tcp-'))
  let server
  try {
    // mcp_pid null (unidentifiable owner) + HTTP port alive: the TCP probe
    // is ANNOTATION ONLY — it must NOT resurrect a failed identity gate
    // (any process can hold the recorded port after the server died). The
    // render is the honest disconnected fallback with a "port held" note,
    // never the stale healthy snapshot.
    server = net.createServer((s) => s.end())
    const port = await new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server.address().port)))

    const project = join(tmp, 'proj')
    writeInventory(project, {
      schema_version: 1,
      written_by: 'dutabo',
      mcp_pid: null,
      mcp_http_port: port,
      duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
    })
    const { ctx, calls } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, { tcpTimeoutMs: 200, autoStart: false })
      await sleep(300)
      check(
        'mcp_pid null + live port → disconnected fallback, port annotated (TCP never gates liveness)',
        calls.length >= 1 && calls.at(-1).text === '✗ alpha:disconnected (port held)',
        JSON.stringify(calls),
      )
    } finally {
      process.chdir(prevCwd)
    }

    // port dead → clear
    {
    const port2 = await new Promise((resolve) => {
      const s = net.createServer()
      s.listen(0, '127.0.0.1', () => {
        const port = s.address().port
        s.close(() => resolve(port))
      })
    })
    writeInventory(project, {
      schema_version: 1,
      written_by: 'dutabo',
      mcp_pid: null,
      mcp_http_port: port2,
      duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
    })
    const { ctx, calls } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, { tcpTimeoutMs: 200, autoStart: false })
      await sleep(300)
      check(
        'dead HTTP port renders the disconnected fallback',
        calls.at(-1).text === '✗ alpha:disconnected',
        JSON.stringify(calls),
      )
    } finally {
      process.chdir(prevCwd)
    }
    }
  } finally {
    if (server) server.close()
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  // no project → first tick clears (never throws)
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-none-'))
  try {
    const { ctx, calls } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(tmp)
    try {
      apply(ctx, {})
      await sleep(80)
      check('no project → contribution cleared', calls.length >= 1 && calls.at(-1).text === undefined, JSON.stringify(calls))
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  // The watchdog (pollIntervalMs) must fire repeatedly and keep re-arming
  // the watcher: it is the safety net for a watcher killed by an error and
  // for freshness-aging after a silent server death (kill -9 emits NO fs
  // event). Before the watchdog existed, `arms` stayed frozen at 1.
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-watchdog-'))
  const fake = await spawnFakeMcp()
  try {
    const project = join(tmp, 'proj')
    writeInventory(project, {
      schema_version: 1,
      mcp_pid: fake.pid,
      heartbeat_secs: 30,
      written_at_unix: Math.floor(Date.now() / 1000),
      duts: [{ dut_name: 'a', state: 'active', state_text: '\x1b[32m● a:active\x1b[0m' }],
    })
    let arms = 0
    const watchImpl = (dir, cb) => {
      arms += 1
      return watch(dir, cb)
    }
    const { ctx } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, { pollIntervalMs: 60, autoStart: false, watchImpl })
      const before = arms
      await sleep(250)
      check(
        'watchdog fires on pollIntervalMs and re-arms the watcher',
        arms >= before + 2,
        `watcher arms ${before} → ${arms} after 250ms (pollIntervalMs=60)`,
      )
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    fake.kill()
    rmSync(tmp, { recursive: true, force: true })
  }
}

{
  // session switch (tui/session-switched) → re-target the project by new cwd
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-switch-'))
  const fake = await spawnFakeMcp()
  try {
    const projectA = join(tmp, 'proj-a')
    const projectB = join(tmp, 'proj-b')
    writeInventory(projectA, {
      schema_version: 1,
      mcp_pid: fake.pid,
      heartbeat_secs: 30,
      written_at_unix: Math.floor(Date.now() / 1000),
      duts: [{ dut_name: 'a', state: 'active', state_text: '\x1b[32m● a:active\x1b[0m' }],
    })
    writeInventory(projectB, {
      schema_version: 1,
      mcp_pid: fake.pid,
      heartbeat_secs: 30,
      written_at_unix: Math.floor(Date.now() / 1000),
      duts: [{ dut_name: 'b', state: 'crashed', state_text: '\x1b[31m✗ b:crashed\x1b[0m' }],
    })
    const { ctx, calls, getTeardown, disposers, handlers } = makeMockCtx()
    const prevCwd = process.cwd()
    process.chdir(projectA)
    try {
      apply(ctx, { autoStart: false })
      check('renders initial workspace project', calls.at(-1)?.text === '● a:active', JSON.stringify(calls.at(-1)))

      handlers.get('tui/session-switched')({ cwd: projectB })
      await sleep(30)
      check(
        'session switch re-targets to new cwd project',
        calls.at(-1)?.text === '✗ b:crashed',
        JSON.stringify(calls.at(-1)),
      )

      getTeardown()()
      check('unload unsubscribes the event', disposers.includes('offed:tui/session-switched'))
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    fake.kill()
    rmSync(tmp, { recursive: true, force: true })
  }
}

// Visitor text is generated by Rust; clients select it without reformatting.
{
  const inv = { duts: [
    { state_text: 'OWNER_ACTIVE', guest_display: { text: 'SERVER_BUSY' } },
    { state_text: 'SERVER_DUTABO', guest_display: { text: 'SERVER_DUTABO' } },
  ] }
  check('owner keeps real state and dutabo', renderStatus(inv) === 'OWNER_ACTIVE · SERVER_DUTABO')
  check('guest selects busy and dutabo from server', renderStatus(inv, true) === 'SERVER_BUSY · SERVER_DUTABO')
}


console.log(failed === 0 ? 'ALL PASS' : `${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)

