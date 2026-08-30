// Run with: node policy.mjs /absolute/path/to/node_modules/jiti/lib/jiti.cjs
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { createJiti } = require(process.argv[2] ?? "jiti");
const jiti = createJiti(import.meta.url);
const temp = fs.mkdtempSync(path.join(os.tmpdir(), "sermcp-pi-policy-"));
const oldConfig = process.env.TARGET_CONF;
const oldBinary = process.env.SERMCP_BIN;
try {
  const config = path.join(temp, ".target.jsonc");
  const marker = path.join(temp, ".dut-serial", "stopped");
  fs.writeFileSync(config, "{}");
  fs.mkdirSync(path.dirname(marker));
  fs.writeFileSync(marker, "human stop");
  process.env.TARGET_CONF = config;
  process.env.SERMCP_BIN = "/bin/true";
  const handlers = new Map();
  let registrations = 0;
  const extension = await jiti.import(fileURLToPath(new URL("../index.ts", import.meta.url)), { default: true });
  extension({
    on: (name, callback) => handlers.set(name, callback),
    registerMcpServer: () => { registrations++; },
    registerTool: () => {},
    getAllTools: () => [],
  });
  assert.equal(registrations, 0, "manual stop inhibits registration");
  const ctx = { cwd: temp, ui: { notify: () => {} } };
  const call = (command) => handlers.get("tool_call")({ toolName: "bash", input: { command } }, ctx);
  for (const command of ["dutabo stop", "dutabo resume", "dutabo start --agent", "sermcp --http --idle-timeout 0"]) {
    assert.equal(call(command)?.block, true, command);
  }
  assert.equal(call("echo 'dutabo stop'"), undefined);
  assert.equal(call("kill -TERM 12345"), undefined);
  handlers.get("before_agent_start")({}, ctx);
  assert.equal(registrations, 0);
  fs.unlinkSync(marker);
  handlers.get("before_agent_start")({}, ctx);
  assert.equal(registrations, 1, "human resume permits registration on the next turn");
  for (const command of ["dutabo start --agent", "sermcp --http --idle-timeout 0"]) {
    assert.equal(call(command), undefined, command);
  }
  assert.equal(call("dutabo stop")?.block, true);
  assert.equal(call("dutabo start")?.block, true);
  console.log("pi lifecycle policy: passed");
} finally {
  if (oldConfig === undefined) delete process.env.TARGET_CONF;
  else process.env.TARGET_CONF = oldConfig;
  if (oldBinary === undefined) delete process.env.SERMCP_BIN;
  else process.env.SERMCP_BIN = oldBinary;
  fs.rmSync(temp, { recursive: true, force: true });
}
