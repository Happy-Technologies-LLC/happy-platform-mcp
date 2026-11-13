import { createWorld, closeWorld } from './lib/world.mjs';
import { runTools } from './stages/tools.mjs';
const only = process.argv.slice(2);
const world = await createWorld();
try {
  const out = await runTools(world, { only: only.length ? only : undefined });
  for (const [file, doc] of out) {
    console.log('=====', file);
    for (const s of doc.steps) {
      console.log('REQ', JSON.stringify(s.request.params).slice(0, 300));
      for (const o of s.outbound) console.log('  >>', o.method, o.path + (o.rawQuery ? '?' + o.rawQuery : ''), JSON.stringify(o.body ?? o.form ?? '').slice(0, 200));
      const text = s.response.result?.content?.[0]?.text ?? JSON.stringify(s.response);
      console.log('RES', s.response.result?.isError ? '[isError]' : s.response.error ? '[rpc error]' : '', text.slice(0, 500).replace(/\n/g, '\\n'));
      if (s.notifications.length) console.log('NOTIF', s.notifications.length);
      if (Object.keys(s.files).length) console.log('FILES', Object.keys(s.files));
    }
    if (doc.harnessError) console.log('HARNESS ERROR', doc.harnessError);
  }
} finally { await closeWorld(world); }
