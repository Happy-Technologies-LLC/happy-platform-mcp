import http from 'node:http';

export const PINNED_RAW_ORIGIN = 'https://raw.githubusercontent.com';
export const REPO_PREFIX = '/ServiceNow/ServiceNowDocs';
const realFetch = globalThis.fetch;

export function mainIndex(families = [['australia', 'australia']]) {
  return [
    '# ServiceNow Product Documentation',
    '',
    '- Family-to-branch mapping is:',
    ...families.map(([name, branch]) => `    - "${name}" : "${branch}" -- ${name} family`),
    ''
  ].join('\n');
}

/**
 * Local stand-in for raw.githubusercontent.com. Routes map an exact request
 * path (as received on the wire) to a handler or a static body.
 */
export async function startFakeRawGitHub(routes = {}) {
  const requests = [];
  const sockets = new Set();
  const server = http.createServer((req, res) => {
    requests.push({ method: req.method, url: req.url, headers: { ...req.headers } });
    const route = routes[req.url];
    if (route === undefined) {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('not found');
      return;
    }
    if (typeof route === 'function') {
      route(req, res);
      return;
    }
    res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    res.end(route);
  });
  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;

  return {
    routes,
    requests,
    base,
    /**
     * Test-only fetch: the production client still builds pinned
     * https://raw.githubusercontent.com URLs; only this wrapper forwards them
     * to the loopback server, preserving the path and request init (including
     * the redirect policy and abort signal).
     */
    fetchImpl(url, init) {
      const text = String(url);
      if (!text.startsWith(`${PINNED_RAW_ORIGIN}/`)) {
        throw new Error(`Unpinned docs URL requested: ${text}`);
      }
      return realFetch(base + text.slice(PINNED_RAW_ORIGIN.length), init);
    },
    paths() {
      return requests.map((request) => request.url);
    },
    async close() {
      for (const socket of sockets) socket.destroy();
      await new Promise((resolve) => server.close(resolve));
    }
  };
}
