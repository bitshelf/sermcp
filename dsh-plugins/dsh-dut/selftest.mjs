#!/usr/bin/env node
/**
 * dsh-dut selftest — same style as the Python hooks' --selftest.
 *
 * Run: node dsh-plugins/dsh-dut/selftest.mjs
 * Covers:
 *   - statusline half: renderStatus/parseAnsiSegments/stripAnsi/
 *     renderDisconnected, project discovery, inventory, port mirror,
 *     auto-start helpers, ownership, mock-ctx integration (live pid, state
 *     change, dead-server fallback, styled seam, unload cleanup);
 *   - mcp half: publicToolName/extractText/createDefinition shapes,
 *     syncTools against a REAL fake MCP server (SDK Server over Streamable
 *     HTTP), applyMcp end-to-end: connect → tools registered on ctx.tools →
 *     execute forwards tools/call → server death unregisters honestly →
 *     unload cleanup.
 */
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import net from 'node:net'
import http from 'node:http'
import { spawn } from 'node:child_process'

import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js'
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from '@modelcontextprotocol/sdk/types.js'

import {
  MCP_DEFAULTS,
  STATUSLINE_DEFAULTS,
  apply,
  applyMcp,
  canonicalDir,
  checkDir,
  createDefinition,
  dutPort,
  ensureServer,
  extractText,
  findProjectDir,
  isGuest,
  inventoryLive,
  loadInventory,
  loadOwner,
  mcpPortFor,
  ownerIsAlive,
  parseAnsiSegments,
  pidAlive,
  pidIsMcpServer,
  portAlive,
  projectPort,
  publicToolName,
  renderDisconnected,
  renderStatus,
  resolveServerBinary,
  serverCommand,
  serverEnv,
  stripAnsi,
  syncTools,
} from './index.js'

let failed = 0
const check = (label, cond, detail = '') => {
  if (cond) console.log(`OK    ${label}`)
  else {
    console.log(`FAIL  ${label}${detail ? ` — ${detail}` : ''}`)
    failed += 1
  }
}

// ── Statusline: pure functions ────────────────────────────────────────────

{
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
    'renderStatus joins non-empty state_text verbatim',
    renderStatus(inv) === `${activeText} · ${crashedText}`,
    JSON.stringify(renderStatus(inv)),
  )
  check('renderStatus without duts renders empty', renderStatus({ duts: [] }) === '')
  check('renderStatus without inventory renders empty', renderStatus(null) === '')
}

{
  check(
    'parseAnsiSegments maps SGR colors to the segment palette',
    JSON.stringify(parseAnsiSegments('\x1b[32m● a:active\x1b[0m')) ===
      JSON.stringify([{ text: '● a:active', fg: 'ansi:green' }]),
  )
  check(
    'parseAnsiSegments keeps multi-run color resets',
    JSON.stringify(parseAnsiSegments('\x1b[32m● a:active\x1b[0m · \x1b[31m✗ b:crashed\x1b[0m')) ===
      JSON.stringify([
        { text: '● a:active', fg: 'ansi:green' },
        { text: ' · ' },
        { text: '✗ b:crashed', fg: 'ansi:red' },
      ]),
  )
  check(
    'parseAnsiSegments bright codes map to the bright palette',
    parseAnsiSegments('\x1b[91m✗ a\x1b[0m')[0]?.fg === 'ansi:redBright',
  )
  check('parseAnsiSegments empty → []', JSON.stringify(parseAnsiSegments('')) === '[]')
  check(
    'stripAnsi removes SGR codes',
    stripAnsi('\x1b[32m● a:active\x1b[0m') === '● a:active',
  )
}

{
  check(
    'renderDisconnected joins dut names as red ✗ name:disconnected',
    renderDisconnected({ duts: [{ dut_name: 'a' }, { dut_name: 'b', state_plain: 'x' }, { dut_name: '' }] }) ===
      '\x1b[31m✗ a:disconnected\x1b[0m · \x1b[31m✗ b:disconnected\x1b[0m',
  )
  check('renderDisconnected without duts renders empty', renderDisconnected({ duts: [] }) === '')
  check('renderDisconnected null renders empty', renderDisconnected(null) === '')
}

// ── Port mirror (must stay byte-identical to sermcp/src/ports.rs) ──────────

{
  check('projectPort matches ports.rs mirror (/work/dut-project → 3003)', projectPort('/work/dut-project') === 3003)
  check('dutPort matches ports.rs mirror (/work/dut-project:alpha → 3098)', dutPort('/work/dut-project', 'alpha') === 3098)
  check('mcpPortFor prefers the recorded inventory port', mcpPortFor('/work/dut-project', { mcp_http_port: 3017 }) === 3017)
  check('mcpPortFor falls back to the current_dut port', mcpPortFor('/work/dut-project', { current_dut: 'alpha' }) === 3098)
  check('mcpPortFor falls back to the first dut port', mcpPortFor('/work/dut-project', { duts: [{ dut_name: 'alpha' }] }) === 3098)
  check('mcpPortFor without inventory falls back to the project port', mcpPortFor('/work/dut-project', null) === 3003)
  check('canonicalDir resolves symlinks', canonicalDir('/work/dut-project') === '/work/dut-project')
}

// ── Auto-start helpers (binary lookup + spawn shape) ───────────────────────

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-bin-'))
  try {
    const fakeBin = join(tmp, 'fake-sermcp')
    writeFileSync(fakeBin, '')
    check('resolveServerBinary honors an explicit existing override', resolveServerBinary(fakeBin) === fakeBin)
    check('resolveServerBinary rejects a missing override', resolveServerBinary(join(tmp, 'nope')) === null)

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

// ── Project discovery + inventory ─────────────────────────────────────────

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

    check('checkDir rejects forbidden roots', checkDir('/tmp') === null && checkDir('/') === null)

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
    check('ownerIsAlive false with dead anchor', !ownerIsAlive(loadOwner(project)))
    check('isGuest empty when owner dead', isGuest(loadOwner(project)) === false)

    writeOwner({ schema_version: 1, owner_mcp_pid: null, owner_pid: 424242, claimed_at_unix: Math.floor(Date.now() / 1000) })
    check('ownerIsAlive honors claim grace', ownerIsAlive(loadOwner(project)))
    writeOwner({ schema_version: 1, owner_mcp_pid: null, owner_pid: 424242, claimed_at_unix: Math.floor(Date.now() / 1000) - 120 })
    check('ownerIsAlive false after grace expires', !ownerIsAlive(loadOwner(project)))
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

// ── MCP half: pure functions ───────────────────────────────────────────────

{
  check(
    'publicToolName builds mcp__<server>__<raw>',
    publicToolName('dut', 'serial_get_state') === 'mcp__dut__serial_get_state',
  )
  check(
    'extractText joins text blocks with newlines',
    extractText({ content: [{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }] }) === 'a\nb',
  )
  check(
    'extractText serializes non-text blocks',
    extractText({ content: [{ type: 'resource', uri: 'x' }] }) === JSON.stringify({ type: 'resource', uri: 'x' }),
  )
  check(
    'extractText falls back to toolResult',
    extractText({ toolResult: { ok: 1 } }) === JSON.stringify({ ok: 1 }),
  )
  check('extractText empty content → empty string', extractText({ content: [] }) === '')
  check('extractText null → JSON null', extractText(null) === 'null')
}

{
  // createDefinition shape + execute forwarding (mock client, no network).
  const calls = []
  const mockClient = {
    async callTool(request, resultSchema, options) {
      calls.push({ request, resultSchema, options })
      return { content: [{ type: 'text', text: `echo:${request.arguments.cmd}` }] }
    },
  }
  const def = createDefinition(mockClient, 'mcp__dut__serial_send_command', 'serial_send_command', 'Run a command', { type: 'object', properties: { cmd: { type: 'string' } } }, 1234)
  check('createDefinition carries public name', def.name === 'mcp__dut__serial_send_command')
  check('createDefinition carries description', def.description === 'Run a command')
  check('createDefinition carries parameters (MCP inputSchema passthrough)', def.parameters.properties.cmd.type === 'string')
  check(
    'createDefinition output declares schema + render',
    def.output?.schema?.type === 'object' && typeof def.output.render === 'function',
  )
  const rendered = def.output.render({}, { content: [{ type: 'text', text: 'hi' }] })
  check('output.render projects text blocks', rendered[0]?.type === 'text' && rendered[0].text === 'hi')
  const value = await def.execute({ cmd: 'uname -a' })
  check(
    'execute forwards tools/call with the RAW name and timeout',
    calls[0].request.name === 'serial_send_command' && calls[0].options.timeout === 1234 &&
      calls[0].request.arguments.cmd === 'uname -a',
    JSON.stringify(calls[0]),
  )
  check('execute returns canonical { content }', value.content[0].text === 'echo:uname -a')

  const errClient = {
    async callTool() {
      return { content: [{ type: 'text', text: 'boom' }], isError: true }
    },
  }
  const errDef = createDefinition(errClient, 'x', 'x', '', {})
  let threw = null
  try {
    await errDef.execute({})
  } catch (e) {
    threw = e
  }
  check('execute throws on isError (ToolRuntime renders failures)', threw !== null && threw.message === 'boom')
}

// ── MCP half: syncTools against a REAL fake MCP server ─────────────────────

async function startFakeMcpServer(tools) {
  const mcpServer = new Server(
    { name: 'fake-sermcp', version: '0.0.0' },
    { capabilities: { tools: {} } },
  )
  mcpServer.registerCapabilities({ tools: {} })
  mcpServer.setRequestHandler(
    ListToolsRequestSchema,
    async () => ({ tools }),
  )
  mcpServer.setRequestHandler(
    CallToolRequestSchema,
    async (request) => {
      const name = request.params.name
      const args = request.params.arguments ?? {}
      const tool = tools.find((t) => t.name === name)
      if (!tool) {
        return { content: [{ type: 'text', text: `unknown tool ${name}` }], isError: true }
      }
      return {
        content: [{ type: 'text', text: `handled:${name}:${JSON.stringify(args)}` }],
        structuredContent: { handled: name },
      }
    },
  )

  const sessions = new Map()
  let transport = null
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1')
    if (url.pathname !== '/mcp') {
      res.writeHead(404).end()
      return
    }
    const body = await new Promise((resolveBody) => {
      const chunks = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => resolveBody(Buffer.concat(chunks)))
    })
    // handleRequest's parsedBody expects the PARSED JSON object (body-parser
    // semantics), not raw bytes.
    let parsed
    try {
      parsed = body.length > 0 ? JSON.parse(body.toString('utf8')) : undefined
    } catch {
      parsed = undefined
    }
    if (req.method === 'POST') {
      if (!transport) {
        // Single-session fake: the first POST creates the session.
        transport = new StreamableHTTPServerTransport({
          sessionIdGenerator: () => 'sess-1',
          onsessioninitialized: () => {},
        })
        sessions.set(transport.sessionId, transport)
        await mcpServer.connect(transport)
      }
      await transport.handleRequest(req, res, parsed)
    } else if (req.method === 'GET' && (req.headers.accept ?? '').includes('text/event-stream')) {
      const t = sessions.get(String(req.headers['mcp-session-id'] ?? ''))
      if (!t) {
        res.writeHead(400).end('no session')
        return
      }
      await t.handleRequest(req, res, parsed)
    } else {
      res.writeHead(405).end()
    }
  })
  await new Promise((resolveListen) => server.listen(0, '127.0.0.1', resolveListen))
  const port = server.address().port

  return {
    port,
    sessions,
    mcpServer,
    async close() {
      for (const t of sessions.values()) await t.close()
      sessions.clear()
      await new Promise((r) => server.close(r))
    },
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function waitFor(cond, timeoutMs = 5000, label = 'condition') {
  const start = Date.now()
  while (Date.now() - start < timeoutMs) {
    if (cond()) return true
    await sleep(30)
  }
  return false
}

function makeBridgeCtx() {
  const registered = new Map() // publicName → definition
  const disposals = []
  const handlers = new Map()
  let teardown = null
  const ctx = {
    tools: {
      register(definition) {
        registered.set(definition.name, definition)
        return () => {
          disposals.push(definition.name)
          registered.delete(definition.name)
        }
      },
    },
    tuiStatus: { set() { return () => {} } },
    logger: { debug: () => {}, warn: () => {}, error: () => {} },
    on(name, handler) {
      handlers.set(name, handler)
      return () => {}
    },
    effect(fn) {
      teardown = fn()
      return teardown
    },
  }
  return { ctx, registered, disposals, handlers, getTeardown: () => teardown }
}

{
  const fake = await startFakeMcpServer([
    { name: 'serial_get_state', description: 'Get DUT state', inputSchema: { type: 'object', properties: { dut_name: { type: 'string' } } } },
    { name: 'serial_send_command', description: 'Run a command', inputSchema: { type: 'object', properties: { command: { type: 'string' }, dut_name: { type: 'string' } } } },
    { name: 'serial_list_logs', description: 'List logs', inputSchema: { type: 'object', properties: {} } },
  ])
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-bridge-'))
  try {
    const project = join(tmp, 'proj')
    mkdirSync(project, { recursive: true })
    writeFileSync(join(project, '.target.jsonc'), '')
    writeInventory(project, {
      schema_version: 1,
      written_by: 'mcp-server',
      mcp_pid: 1 << 30, // dead pid → the TCP probe decides liveness
      mcp_http_port: fake.port,
      current_dut: 'alpha',
      duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
    })
    const { ctx, registered, disposals, getTeardown } = makeBridgeCtx()
    const prevCwd = process.cwd()
    process.chdir(project)
    try {
      apply(ctx, { role: 'mcp', pollIntervalMs: 50, tcpTimeoutMs: 300, autoStart: false, serverName: 'dut' })

      const connected = await waitFor(() => registered.has('mcp__dut__serial_get_state'), 5000, 'bridge connection')
      check('applyMcp connects and registers tools', connected, `registered: ${JSON.stringify([...registered.keys()])}`)
      check(
        'every serial_* tool registers under mcp__dut__*',
        ['serial_get_state', 'serial_send_command', 'serial_list_logs'].every((n) => registered.has(`mcp__dut__${n}`)),
        JSON.stringify([...registered.keys()]),
      )
      const def = registered.get('mcp__dut__serial_send_command')
      check('registered definition carries the server description', def?.description === 'Run a command')
      check('registered definition carries parameters', def?.parameters?.properties?.command?.type === 'string')

      // execute → tools/call round-trip through the fake server
      const value = await def.execute({ command: 'uname -a', dut_name: 'alpha' })
      check(
        'execute round-trips tools/call (structuredContent preserved)',
        value.content[0].text === 'handled:serial_send_command:{"command":"uname -a","dut_name":"alpha"}' &&
          value.structuredContent?.handled === 'serial_send_command',
        JSON.stringify(value),
      )

      // toolPattern filters registration
      const filtered = await syncTools(
        { async listTools() { return { tools: [{ name: 'serial_get_state' }, { name: 'serial_reset' }] } } },
        makeBridgeCtx().ctx,
        { serverName: 'dut', toolPattern: '^serial_(get|list)_' },
      )
      check(
        'syncTools toolPattern registers only matches',
        [...filtered.keys()].join(',') === 'mcp__dut__serial_get_state',
        JSON.stringify([...filtered.keys()]),
      )
      for (const dispose of filtered.values()) dispose()

      // server death → honest unregister (never a stale tool)
      const fakePort = fake.port
      await fake.close()
      const unregistered = await waitFor(() => disposals.includes('mcp__dut__serial_get_state'), 5000, 'unregister after server death')
      check('server death unregisters tools honestly', unregistered, `disposals: ${JSON.stringify(disposals)}`)
      check('all tools unregistered after death', registered.size === 0, JSON.stringify([...registered.keys()]))

      // cleanup
      getTeardown()()
      await sleep(100)
      check('unload leaves nothing registered', registered.size === 0 && disposals.length >= 3)

      // fake server gone — port liveness probe must be false now
      check('portAlive false after server close', !(await portAlive(fakePort, 200)))
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    rmSync(tmp, { recursive: true, force: true })
    await fake.close().catch(() => {})
  }
}

// ── MCP half: no project → dormant, no connection attempts ─────────────────

{
  const tmp = mkdtempSync(join(tmpdir(), 'dsh-dut-selftest-dormant-'))
  try {
    const { ctx, registered, getTeardown } = makeBridgeCtx()
    const prevCwd = process.cwd()
    process.chdir(tmp) // no .target.jsonc, no .dut-serial anywhere up
    try {
      apply(ctx, { role: 'mcp', pollIntervalMs: 30, autoStart: false })
      await sleep(150)
      check('mcp role stays dormant outside a configured project', registered.size === 0)
      getTeardown()()
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

// ── Statusline half: mock-ctx integration ──────────────────────────────────

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
    tuiStatus.setStyled = (key, segments, identity) => {
      calls.push({ key, segments, identity })
      const dispose = () => disposers.push('disposed')
      return dispose
    }
  }
  const ctx = {
    tuiStatus,
    tools: { register() { return () => {} } },
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
      apply(ctx, { role: 'statusline', tcpTimeoutMs: 100, autoStart: false })
      check(
        'live pid renders immediately (first render synchronous, no setStyled → ANSI-stripped plain text)',
        calls.length === 1 && calls[0].key === 'dut-statusline' && calls[0].text === '● alpha:active',
        JSON.stringify(calls),
      )
      check('identity passed as plugin ctx', calls[0].identity === ctx)

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

      writeInventory(project, {
        schema_version: 1,
        written_by: 'mcp-server',
        mcp_pid: 1 << 30,
        mcp_http_port: null,
        duts: [{ dut_name: 'alpha', state: 'active', state_text: '\x1b[32m● alpha:active\x1b[0m' }],
      })
      await sleep(150)
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
      apply(ctx, { role: 'statusline', autoStart: false })
      check(
        'styled ctx receives parsed segments (setStyled path)',
        calls.length === 1 &&
          JSON.stringify(calls[0].segments) ===
            JSON.stringify([
              { text: '● alpha:active', fg: 'ansi:green' },
              { text: ' · ' },
              { text: '✗ beta:crashed', fg: 'ansi:red' },
            ]),
        JSON.stringify(calls[0]?.segments),
      )
      getTeardown()()
    } finally {
      process.chdir(prevCwd)
    }
  } finally {
    fake.kill()
    rmSync(tmp, { recursive: true, force: true })
  }
}

// ── Role dispatch + defaults ───────────────────────────────────────────────

{
  check('STATUSLINE_DEFAULTS role', STATUSLINE_DEFAULTS.role === 'statusline')
  check('MCP_DEFAULTS role', MCP_DEFAULTS.role === 'mcp' && MCP_DEFAULTS.serverName === 'dut')
  check('MCP_DEFAULTS reconnect backoff sane', MCP_DEFAULTS.reconnectInitialMs <= MCP_DEFAULTS.reconnectMaxMs)
}

// ── Upgrade-resilience import surface ──────────────────────────────────────
// The bridge must survive dsh/dsh-tui upgrades: runtime imports are limited
// to node builtins + @modelcontextprotocol/sdk (a frozen protocol library).
// Any @deepseek-ai/* runtime import would pin a dsh core version line and
// break on the next upgrade — the exact failure the README's 升级兼容性
// section forbids.

{
  const src = readFileSync(join(import.meta.dirname, 'index.js'), 'utf8')
  const imports = [...src.matchAll(/^\s*import\s+(?:[^'"]*?\s+from\s+)?['"]([^'"]+)['"]/gm)]
    .map(match => match[1])
  const allowed = new Set([
    '@modelcontextprotocol/sdk/client/index.js',
    '@modelcontextprotocol/sdk/client/streamableHttp.js',
  ])
  const offenders = imports.filter(spec => !spec.startsWith('node:') && !allowed.has(spec))
  check(
    'import surface: only node builtins + @modelcontextprotocol/sdk',
    offenders.length === 0,
    offenders.join(', '),
  )
  const deepseekImports = imports.filter(spec => spec.startsWith('@deepseek-ai/'))
  check(
    'no @deepseek-ai/* runtime imports (upgrade-resilience contract)',
    deepseekImports.length === 0,
    deepseekImports.join(', '),
  )
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

if (failed > 0) {
  console.error(`\n${failed} check(s) FAILED`)
  process.exit(1)
}
console.log('\nAll checks OK')

