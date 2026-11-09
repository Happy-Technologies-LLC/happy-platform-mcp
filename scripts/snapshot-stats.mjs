#!/usr/bin/env node
/**
 * Persist usage data that the upstream APIs do not keep:
 *   - GitHub traffic (views/clones): API returns only the last 14 days, per day.
 *   - Docker Hub pulls: API returns only a lifetime counter, no history.
 *   - skills.sh installs: no API or history; read from the public repo page.
 * npm is also recorded for convenience, though npm keeps full daily history itself.
 *
 * Writes to $HAPPY_STATS_DIR (default ~/.local/share/happy-platform-mcp-stats):
 *   traffic-daily.json  { <owner/repo>: { views: {YYYY-MM-DD: {count, uniques}}, clones: {...} } }  merged each run
 *   snapshots.jsonl     one line per run with counters at that moment
 *
 * Run daily (launchd: ~/Library/LaunchAgents/com.nczitzer.happy-mcp-stats.plist).
 * GitHub auth: GITHUB_TOKEN/GH_TOKEN, else `gh auth token`.
 */
import { execFileSync } from 'node:child_process';
import { appendFileSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const REPOS = ['Happy-Technologies-LLC/happy-platform-mcp', 'Happy-Technologies-LLC/happy-platform-skills'];
const NPM = ['happy-platform-mcp', 'servicenow-mcp-server', 'happy-platform-skills', 'happy-servicenow-skills'];
const DOCKER = ['nczitzer/happy-platform-mcp', 'nczitzer/servicenow-mcp-server', 'nczitzer/mcp-servicenow-nodejs'];
const DIR = process.env.HAPPY_STATS_DIR || join(homedir(), '.local/share/happy-platform-mcp-stats');

function ghToken() {
  if (process.env.GITHUB_TOKEN || process.env.GH_TOKEN) return process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  for (const bin of ['/opt/homebrew/bin/gh', '/usr/local/bin/gh', 'gh']) {
    try {
      return execFileSync(bin, ['auth', 'token'], { encoding: 'utf8' }).trim();
    } catch {}
  }
  throw new Error('No GitHub token: set GITHUB_TOKEN or run `gh auth login`');
}

async function getJson(url, headers = {}) {
  const res = await fetch(url, { headers: { accept: 'application/json', ...headers } });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  return res.json();
}

const token = ghToken();
const gh = (path) => getJson(`https://api.github.com/${path}`, { authorization: `Bearer ${token}` });

const ghData = await Promise.all(
  REPOS.map(async (r) => {
    const [views, clones, repo] = await Promise.all([
      gh(`repos/${r}/traffic/views`),
      gh(`repos/${r}/traffic/clones`),
      gh(`repos/${r}`),
    ]);
    return { views, clones, repo };
  })
);
const docker = await Promise.all(
  DOCKER.map((r) => getJson(`https://hub.docker.com/v2/repositories/${r}/`).catch(() => ({ pull_count: null })))
);
// skills.sh has no API; sum the exact per-skill install counts from the repo page.
const SKILLS_SH = 'https://skills.sh/happy-technologies-llc/happy-platform-skills';
const skillsShHtml = await fetch(SKILLS_SH).then((r) => (r.ok ? r.text() : ''));
const skillRows = [
  ...skillsShHtml.matchAll(
    /href="\/happy-technologies-llc\/happy-platform-skills\/([^"]+)".{0,1200}?<h3[^>]*>([^<]+)<\/h3>.{0,500}?<span[^>]*>([\d,]+)<\/span>/gs
  ),
];
// A repo-level `npx skills add` is counted once per skill, so the sum is inflated ~skills-fold.
// max_skill_installs is the closest figure to real installs; sum kept only for reference.
const skillCounts = skillRows.map((m) => Number(m[3].replace(/,/g, '')));
const skillsSh = skillCounts.length
  ? {
      max_skill_installs: Math.max(...skillCounts),
      sum_skill_installs: skillCounts.reduce((sum, n) => sum + n, 0),
      skills: skillCounts.length,
    }
  : null;
const npm = await Promise.all(NPM.map((p) => getJson(`https://api.npmjs.org/downloads/point/last-day/${p}`)));

mkdirSync(DIR, { recursive: true });

// Merge per-day traffic. Later fetches win: the newest day in a response is partial.
const trafficPath = join(DIR, 'traffic-daily.json');
let traffic = {};
try {
  traffic = JSON.parse(readFileSync(trafficPath, 'utf8'));
} catch (err) {
  if (err.code !== 'ENOENT') throw err;
}
REPOS.forEach((r, i) => {
  traffic[r] ??= { views: {}, clones: {} };
  for (const kind of ['views', 'clones']) {
    for (const row of ghData[i][kind][kind] ?? []) {
      traffic[r][kind][row.timestamp.slice(0, 10)] = { count: row.count, uniques: row.uniques };
    }
  }
});
const tmp = `${trafficPath}.tmp`;
writeFileSync(tmp, JSON.stringify(traffic, null, 2) + '\n');
renameSync(tmp, trafficPath);

const snapshot = {
  at: new Date().toISOString(),
  docker_pulls: Object.fromEntries(DOCKER.map((r, i) => [r, docker[i].pull_count])),
  skills_sh: skillsSh,
  npm_last_day: Object.fromEntries(NPM.map((p, i) => [p, { day: npm[i].start, downloads: npm[i].downloads }])),
  github: Object.fromEntries(
    REPOS.map((r, i) => {
      const { views, clones, repo } = ghData[i];
      return [r, {
        stars: repo.stargazers_count,
        forks: repo.forks_count,
        views_14d: { count: views.count, uniques: views.uniques },
        clones_14d: { count: clones.count, uniques: clones.uniques },
      }];
    })
  ),
};
appendFileSync(join(DIR, 'snapshots.jsonl'), JSON.stringify(snapshot) + '\n');

console.log(
  `${snapshot.at} saved: docker ${JSON.stringify(snapshot.docker_pulls)}; traffic days ` +
    REPOS.map((r) => `${r.split('/')[1]} views=${Object.keys(traffic[r].views).length} clones=${Object.keys(traffic[r].clones).length}`).join(', ')
);
