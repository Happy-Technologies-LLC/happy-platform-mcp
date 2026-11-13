// Spawn guard: every JS 6.2.0 process started by the harness must (a) run in a temp cwd with
// no .env (the server loads .env from cwd via dotenv; the repo .env points at a customer
// instance), (b) carry only a loopback instance URL, (c) not inherit SERVICENOW_INSTANCE.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { REPO } from './paths.mjs';

export function assertSafeSpawn({ cwd, env }) {
  const real = fs.realpathSync(cwd);
  const tmp = fs.realpathSync(os.tmpdir());
  if (!real.startsWith(tmp + path.sep)) throw new Error(`golden guard: cwd ${real} is not under the temp dir`);
  if (real === fs.realpathSync(REPO) || real.startsWith(fs.realpathSync(REPO) + path.sep)) throw new Error('golden guard: cwd is inside the repo');
  if (fs.existsSync(path.join(real, '.env'))) throw new Error('golden guard: cwd contains a .env');
  if (env.SERVICENOW_INSTANCE !== undefined) throw new Error('golden guard: SERVICENOW_INSTANCE must not be set');
  for (const key of ['SERVICENOW_INSTANCE_URL', 'SERVICENOW_URL']) {
    if (env[key] === undefined) continue;
    const host = new URL(env[key]).hostname;
    if (host !== '127.0.0.1' && host !== 'localhost') throw new Error(`golden guard: ${key} host ${host} is not loopback`);
  }
  if (env.HAPPY_CONFIG_PATH && !fs.realpathSync(path.dirname(path.resolve(env.HAPPY_CONFIG_PATH))).startsWith(tmp + path.sep)) throw new Error('golden guard: HAPPY_CONFIG_PATH outside temp dir');
}
