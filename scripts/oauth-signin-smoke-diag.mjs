// TEMPORARY diagnostic for the Windows runner (removed before merge):
// which launch paths can actually make a browser hit a loopback URL?
import http from 'node:http';
import { spawn, execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';

const hits = [];
const server = http.createServer((req, res) => { hits.push({ url: req.url, ua: req.headers['user-agent'] }); res.end('ok'); });
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const edge = ['C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe', 'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe'].find(existsSync);

const info = (cmd, args) => { try { return execFileSync(cmd, args, { encoding: 'utf8' }).trim(); } catch (e) { return `ERR ${e.message.split('\n')[0]}`; } };
console.log('session:', info('cmd.exe', ['/d', '/c', 'query session']));
console.log('explorer running:', info('tasklist', ['/FI', 'IMAGENAME eq explorer.exe']));
console.log('http association:', info('cmd.exe', ['/d', '/c', 'assoc .html & ftype htmlfile']));
console.log('edge path:', edge);

async function attempt(label, cmd, args) {
  const before = hits.length;
  try { spawn(cmd, args, { stdio: 'ignore', detached: true, shell: false }).unref(); } catch (e) { console.log(label, 'spawn error', e.message); }
  for (let i = 0; i < 30 && hits.length === before; i += 1) await wait(1000);
  console.log(label, hits.length > before ? `HIT ${JSON.stringify(hits[hits.length - 1])}` : 'no request within 30s');
}

await attempt('explorer.exe', 'explorer.exe', [`${base}/explorer?a=1&b=2`]);
if (edge) {
  await attempt('msedge direct', edge, ['--no-first-run', `${base}/edge?a=1&b=2`]);
  await attempt('msedge headless', edge, ['--headless=new', '--no-first-run', `${base}/headless?a=1&b=2`]);
}
console.log('edge processes:', info('tasklist', ['/FI', 'IMAGENAME eq msedge.exe']).split('\n').length);
server.close();
process.exit(0);
