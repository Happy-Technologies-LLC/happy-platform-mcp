// Stage: tool behaviour. For each scenario, start a fresh JS stdio server whose
// ServiceNow instance is the loopback stub, replay MCP calls, and record
//   MCP request -> outbound HTTP (stub + docs stub) -> MCP result (+ progress notifications, files written, stderr).
import path from 'node:path';
import fs from 'node:fs';
import { spawnMcp } from '../lib/rpc.mjs';
import { CLI_JS } from '../lib/paths.mjs';
import { makeSandbox, listFiles, refreshScrubber } from '../lib/world.mjs';
import { scenarios as allScenarios } from '../scenarios/tools.mjs';

function stripId(message) {
  const { id, jsonrpc, ...rest } = message; // eslint-disable-line no-unused-vars
  return rest;
}

export async function runScenario(world, scenario) {
  const box = makeSandbox(world, scenario.id, { instances: scenario.instances, env: scenario.env });
  if (scenario.keyring) {
    const seeded = Object.fromEntries(Object.entries(scenario.keyring).map(([account, secret]) => [`happy-platform-mcp\u0000${account}`, secret]));
    fs.writeFileSync(box.env.GOLDEN_KEYRING_FILE, JSON.stringify(seeded), { mode: 0o600 });
  }
  world.stub.reset(scenario.stub || null);
  world.raw.take();
  const server = spawnMcp({ script: CLI_JS, args: scenario.args || [], env: box.env, cwd: box.work });
  const steps = [];
  let initError = null;
  try {
    await server.initialize();
    for (const step of scenario.steps) {
      world.stub.take();
      world.raw.take();
      const method = step.rpc?.method ?? 'tools/call';
      const params = step.rpc?.params ?? step.call;
      const { message, notifications } = await server.request(method, params, { timeoutMs: step.timeoutMs ?? 60000 });
      // settle: let any trailing outbound requests land before slicing
      await new Promise(r => setTimeout(r, step.settleMs ?? 50));
      const outbound = [
        ...world.stub.take().map(r => ({ target: 'servicenow', ...r })),
        ...world.raw.take().map(r => ({ target: 'github-raw', ...r }))
      ];
      steps.push({
        request: { method, params },
        outbound,
        response: stripId(message),
        notifications,
        files: listFiles(box.work)
      });
      if (step.after) await step.after({ world, box });
    }
  } catch (error) {
    initError = error.message;
  }
  const closed = await server.close();
  const doc = {
    id: scenario.id,
    tool: scenario.tool,
    family: scenario.family,
    description: scenario.description,
    mode: scenario.mode || (scenario.instances ? 'registry-multi-instance' : 'env-credentials'),
    steps,
    harnessError: initError,
    exit: { code: closed.code, signal: closed.signal },
    stderr: closed.stderr.split('\n').filter(Boolean)
  };
  return doc;
}

export async function runTools(world, { only } = {}) {
  const out = new Map();
  const list = allScenarios.filter(s => !only || only.some(o => s.id.includes(o) || s.family === o));
  for (const scenario of list) {
    refreshScrubber(world);
    const doc = await runScenario(world, scenario);
    const file = path.posix.join('tools', scenario.family, `${scenario.id.replace(/\//g, '__')}.json`);
    out.set(file, world.scrubber.deep(doc));
  }
  return out;
}

export function scenarioList() { return allScenarios; }
