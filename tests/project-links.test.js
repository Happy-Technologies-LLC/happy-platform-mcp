import path from 'node:path';
import { describe, expect, test } from '@jest/globals';
import { lastSeenVersionPath, updatesHint, versionChangeHint, UPDATES_URL } from '../src/project-links.js';

const homeDir = path.join(path.parse(process.cwd()).root, 'example-home');
const file = path.join(homeDir, '.config', 'happy-platform-mcp', 'last-seen-version');

function memoryFs(initial = {}) {
  const files = new Map(Object.entries(initial));
  return {
    files,
    readFileSync(p) {
      if (!files.has(p)) { const e = new Error('ENOENT'); e.code = 'ENOENT'; throw e; }
      return files.get(p);
    },
    mkdirSync() {},
    writeFileSync(p, data) { files.set(p, String(data)); }
  };
}

describe('lastSeenVersionPath', () => {
  test('uses ~/.config by default and XDG_CONFIG_HOME when set', () => {
    expect(lastSeenVersionPath({ env: {}, homeDir })).toBe(file);
    expect(lastSeenVersionPath({ env: { XDG_CONFIG_HOME: '/xdg' }, homeDir }))
      .toBe(path.join('/xdg', 'happy-platform-mcp', 'last-seen-version'));
  });
});

describe('versionChangeHint', () => {
  test('first run prints the updates hint and records the version', () => {
    const fsImpl = memoryFs();
    expect(versionChangeHint({ env: {}, homeDir, version: '6.3.0', fsImpl })).toBe(updatesHint());
    expect(fsImpl.files.get(file)).toBe('6.3.0\n');
  });

  test('same version stays quiet', () => {
    const fsImpl = memoryFs({ [file]: '6.3.0\n' });
    expect(versionChangeHint({ env: {}, homeDir, version: '6.3.0', fsImpl })).toBeNull();
  });

  test('an upgrade names both versions and links the updates list, then goes quiet', () => {
    const fsImpl = memoryFs({ [file]: '6.2.0\n' });
    const hint = versionChangeHint({ env: {}, homeDir, version: '6.3.0', fsImpl });
    expect(hint).toContain('6.2.0 -> 6.3.0');
    expect(hint).toContain(UPDATES_URL);
    expect(versionChangeHint({ env: {}, homeDir, version: '6.3.0', fsImpl })).toBeNull();
  });

  test('HAPPY_MCP_QUIET silences it without touching the file', () => {
    const fsImpl = memoryFs();
    expect(versionChangeHint({ env: { HAPPY_MCP_QUIET: '1' }, homeDir, version: '6.3.0', fsImpl })).toBeNull();
    expect(fsImpl.files.size).toBe(0);
  });

  test('an unwritable config dir never throws', () => {
    const fsImpl = {
      readFileSync() { throw new Error('EACCES'); },
      mkdirSync() { throw new Error('EACCES'); },
      writeFileSync() { throw new Error('EACCES'); }
    };
    expect(versionChangeHint({ env: {}, homeDir, version: '6.3.0', fsImpl })).toBe(updatesHint());
  });
});
