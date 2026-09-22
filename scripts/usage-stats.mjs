#!/usr/bin/env node
/**
 * Usage stats for happy-platform-mcp: npm downloads, GitHub repo/traffic, Docker Hub pulls.
 * Traffic (views/clones) requires GITHUB_TOKEN with push access; skipped otherwise.
 *
 *   node scripts/usage-stats.mjs            # human readable
 *   node scripts/usage-stats.mjs --json     # machine readable
 */

const PKG = 'happy-platform-mcp';
const PKG_OLD = 'servicenow-mcp-server';
const REPO = 'Happy-Technologies-LLC/happy-platform-mcp';
const DOCKER = 'nczitzer/happy-platform-mcp';
const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;

async function getJson(url, headers = {}) {
  const res = await fetch(url, { headers: { accept: 'application/json', ...headers } });
  if (!res.ok) return { __error: `${res.status} ${res.statusText}` };
  return res.json();
}

const gh = (path) =>
  getJson(`https://api.github.com/${path}`, token ? { authorization: `Bearer ${token}` } : {});

const DAY = 86400000;
const iso = (d) => d.toISOString().slice(0, 10);

/**
 * npm's range endpoint serves at most 18 months per request and has no all-time
 * point endpoint, so walk 12-month windows from the package's first publish date.
 * Returns { total, since, monthly } where monthly is a YYYY-MM -> downloads map.
 */
async function allTimeDownloads(pkg) {
  const meta = await getJson(`https://registry.npmjs.org/${pkg}`);
  if (meta.__error) return { total: null, since: null, monthly: {} };
  const created = new Date(meta.time?.created ?? Date.now());
  // npm download stats start 2015-01-10; earlier dates are rejected.
  const start = new Date(Math.max(created.getTime(), Date.UTC(2015, 0, 10)));
  const today = new Date();
  const windows = [];
  for (let from = start; from <= today; from = new Date(from.getTime() + 365 * DAY + DAY)) {
    const to = new Date(Math.min(from.getTime() + 365 * DAY, today.getTime()));
    windows.push(`${iso(from)}:${iso(to)}`);
  }
  const chunks = await Promise.all(
    windows.map((w) => getJson(`https://api.npmjs.org/downloads/range/${w}/${pkg}`))
  );
  const monthly = {};
  let total = 0;
  for (const chunk of chunks) {
    for (const row of chunk.downloads ?? []) {
      total += row.downloads;
      const key = row.day.slice(0, 7);
      monthly[key] = (monthly[key] ?? 0) + row.downloads;
    }
  }
  return { total, since: iso(start), monthly };
}

const [month, range, repo, releases, docker, views, clones, allTime, allTimeOld] = await Promise.all([
  getJson(`https://api.npmjs.org/downloads/point/last-month/${PKG}`),
  getJson(`https://api.npmjs.org/downloads/range/last-month/${PKG}`),
  gh(`repos/${REPO}`),
  gh(`repos/${REPO}/releases?per_page=100`),
  getJson(`https://hub.docker.com/v2/repositories/${DOCKER}/`),
  token ? gh(`repos/${REPO}/traffic/views`) : null,
  token ? gh(`repos/${REPO}/traffic/clones`) : null,
  allTimeDownloads(PKG),
  allTimeDownloads(PKG_OLD),
]);

const daily = Array.isArray(range.downloads) ? range.downloads : [];
const sum = (rows) => rows.reduce((acc, row) => acc + row.downloads, 0);

const stats = {
  npm: {
    downloads_all_time: allTime.total,
    downloads_all_time_since: allTime.since,
    downloads_all_time_previous_package: allTimeOld.total,
    downloads_all_time_combined:
      allTime.total === null ? null : allTime.total + (allTimeOld.total ?? 0),
    downloads_by_month: allTime.monthly,
    downloads_last_month: month.downloads ?? null,
    downloads_last_7_days: sum(daily.slice(-7)),
    downloads_prev_7_days: sum(daily.slice(-14, -7)),
    best_day: daily.reduce((best, row) => (!best || row.downloads > best.downloads ? row : best), null),
  },
  github: {
    stars: repo.stargazers_count ?? null,
    forks: repo.forks_count ?? null,
    open_issues_and_prs: repo.open_issues_count ?? null,
    releases: Array.isArray(releases) ? releases.length : null,
    latest_release: Array.isArray(releases) ? releases[0]?.tag_name ?? null : null,
    release_asset_downloads: Array.isArray(releases)
      ? releases.reduce((acc, r) => acc + (r.assets || []).reduce((a, x) => a + x.download_count, 0), 0)
      : null,
    views_14d: views?.count ?? null,
    unique_visitors_14d: views?.uniques ?? null,
    clones_14d: clones?.count ?? null,
  },
  docker: {
    pulls: docker.pull_count ?? null,
    stars: docker.star_count ?? null,
    last_pushed: docker.last_updated ?? null,
  },
};

if (process.argv.includes('--json')) {
  console.log(JSON.stringify(stats, null, 2));
} else {
  const { npm, github, docker: d } = stats;
  const trend = npm.downloads_prev_7_days
    ? `${(((npm.downloads_last_7_days - npm.downloads_prev_7_days) / npm.downloads_prev_7_days) * 100).toFixed(0)}%`
    : 'n/a';
  console.log(`npm       ${npm.downloads_last_month} last 30d | ${npm.downloads_last_7_days} last 7d (${trend} vs prior 7d)`);
  console.log(
    `npm total ${npm.downloads_all_time} all-time since ${npm.downloads_all_time_since}` +
      ` | ${npm.downloads_all_time_combined} incl. ${PKG_OLD} (${npm.downloads_all_time_previous_package})`
  );
  console.log(`github    ${github.stars} stars | ${github.forks} forks | ${github.open_issues_and_prs} open issues+PRs | ${github.releases} releases (${github.latest_release})`);
  if (github.views_14d !== null) {
    console.log(`traffic   ${github.views_14d} views / ${github.unique_visitors_14d} uniques / ${github.clones_14d} clones (14d)`);
  } else {
    console.log('traffic   set GITHUB_TOKEN (push access) for views/clones');
  }
  console.log(`docker    ${d.pulls} pulls | last push ${d.last_pushed}`);
}
