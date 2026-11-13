// Minimal newline-delimited JSON-RPC client over a child process's stdio.
// Raw wire capture (no SDK) so the golden bytes are exactly what the server wrote.
import { spawn } from 'node:child_process';
import readline from 'node:readline';
import { PRELOAD } from './paths.mjs';
import { assertSafeSpawn } from './guard.mjs';

export const PROTOCOL_VERSION = '2025-06-18';

export function spawnMcp({ script, args = [], env, cwd }) {
  assertSafeSpawn({ cwd, env });
  const child = spawn(process.execPath, ['--import', PRELOAD, script, ...args], {
    env, cwd, stdio: ['pipe', 'pipe', 'pipe']
  });
  let stderr = '';
  child.stderr.on('data', chunk => { stderr += chunk; });
  const pending = new Map();
  const notifications = [];
  const orphans = [];
  let nextId = 1;
  const rl = readline.createInterface({ input: child.stdout });
  rl.on('line', line => {
    let msg;
    try { msg = JSON.parse(line); } catch { orphans.push(line); return; }
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined) && pending.has(msg.id)) {
      const entry = pending.get(msg.id);
      pending.delete(msg.id);
      entry.resolve(msg);
    } else {
      notifications.push(msg);
    }
  });
  const exited = new Promise(resolve => child.on('exit', (code, signal) => resolve({ code, signal })));

  function send(message) {
    child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  async function request(method, params, { timeoutMs = 30000 } = {}) {
    const id = nextId++;
    const mark = notifications.length;
    const response = new Promise((resolve, reject) => {
      pending.set(id, { resolve });
      setTimeout(() => {
        if (pending.delete(id)) reject(new Error(`timeout waiting for ${method} (id ${id})`));
      }, timeoutMs).unref();
    });
    send({ jsonrpc: '2.0', id, method, ...(params === undefined ? {} : { params }) });
    const message = await response;
    return { message, notifications: notifications.slice(mark) };
  }

  return {
    child,
    request,
    notify(method, params) { send({ jsonrpc: '2.0', method, ...(params === undefined ? {} : { params }) }); },
    send,
    orphans,
    notifications,
    async initialize(clientInfo = { name: 'golden-capture', version: '0.0.0' }) {
      const init = await request('initialize', {
        protocolVersion: PROTOCOL_VERSION, capabilities: {}, clientInfo
      });
      send({ jsonrpc: '2.0', method: 'notifications/initialized' });
      return init;
    },
    async close() {
      child.stdin.end();
      const timer = setTimeout(() => child.kill('SIGTERM'), 3000);
      const result = await exited;
      clearTimeout(timer);
      return { ...result, stderr };
    },
    get stderr() { return stderr; }
  };
}
