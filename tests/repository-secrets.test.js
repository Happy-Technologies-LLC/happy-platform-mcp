import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

describe('repository credential hygiene', () => {
  test('does not retain environment backup files', () => {
    const ignoreRules = readFileSync(path.join(repositoryRoot, '.gitignore'), 'utf8');

    expect(existsSync(path.join(repositoryRoot, '.env.backup'))).toBe(false);
    expect(ignoreRules).toMatch(/^\.env\.backup$/m);
  });

  test('startup script requires caller-provided credentials', () => {
    const script = readFileSync(path.join(repositoryRoot, 'start-mcp.sh'), 'utf8');

    expect(script).toContain('${SERVICENOW_PASSWORD:?');
    expect(script).not.toMatch(/^export SERVICENOW_PASSWORD=.+$/m);
    expect(script).not.toMatch(/dev\d+\.service-now\.com/);
  });

  test('setup guide uses non-secret placeholders', () => {
    const guide = readFileSync(path.join(repositoryRoot, 'docs/SETUP_GUIDE.md'), 'utf8');

    expect(guide).toContain('https://your-instance.service-now.com');
    expect(guide).toContain('"SERVICENOW_PASSWORD": "your-password"');
    expect(guide).not.toMatch(/dev\d+\.service-now\.com/);
  });

  test('CI scans full git history and the packed npm tarball with pinned gitleaks', () => {
    const testWorkflow = readFileSync(path.join(repositoryRoot, '.github/workflows/test.yml'), 'utf8');
    const jobStart = testWorkflow.indexOf('\n  secret-scan:\n');
    expect(jobStart).toBeGreaterThan(-1);
    const secretScanJob = testWorkflow.slice(jobStart);
    expect(secretScanJob).toMatch(/fetch-depth: 0\n/);
    expect(secretScanJob).toContain('scripts/secret-scan.sh install "$RUNNER_TEMP/gitleaks"');
    expect(secretScanJob).toContain('run: scripts/secret-scan.sh history');
    expect(secretScanJob.indexOf('run: npm ci')).toBeLessThan(secretScanJob.indexOf('run: scripts/secret-scan.sh package'));

    const publishWorkflow = readFileSync(path.join(repositoryRoot, '.github/workflows/publish.yml'), 'utf8');
    const packageScan = publishWorkflow.indexOf('run: scripts/secret-scan.sh package');
    expect(packageScan).toBeGreaterThan(publishWorkflow.indexOf('run: npm run test:package'));
    expect(packageScan).toBeLessThan(publishWorkflow.indexOf('run: npm publish'));

    const scanner = readFileSync(path.join(repositoryRoot, 'scripts/secret-scan.sh'), 'utf8');
    expect(scanner).toMatch(/^GITLEAKS_VERSION="\d+\.\d+\.\d+"$/m);
    expect(scanner).toMatch(/^GITLEAKS_LINUX_X64_SHA256="[0-9a-f]{64}"$/m);
    expect(scanner).toContain('sha256sum --check --strict');
    expect(scanner).toContain('--log-opts="--full-history HEAD"');
    expect(scanner).toContain('--redact');
  });

  test('secret-scan baseline only ignores exact historical fingerprints', () => {
    const baseline = readFileSync(path.join(repositoryRoot, '.gitleaksignore'), 'utf8');
    const entries = baseline.split('\n').filter((line) => line.trim() && !line.startsWith('#'));

    expect(entries.length).toBeGreaterThan(0);
    for (const entry of entries) {
      expect(entry).toMatch(/^[0-9a-f]{40}:[^:\s]+:[a-z0-9-]+:\d+$/);
      expect(baseline.slice(0, baseline.indexOf(entry))).toContain('#67');
    }
  });
});
