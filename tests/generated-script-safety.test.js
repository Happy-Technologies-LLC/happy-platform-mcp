/**
 * Generated fix-script containment and generated-JavaScript data safety
 * (issue #67: VULN-008, VULN-009, VULN-010).
 *
 * Every scenario runs through real MCP tool dispatch (SDK Client over an
 * in-memory transport) and, where ServiceNow is involved, a real
 * ServiceNowClient talking to a local fake ServiceNow HTTP endpoint that
 * only captures payloads. Generated scripts are compiled with `vm.Script`
 * (compile only — never run) and their untrusted values are proven to be
 * single JavaScript string literals.
 */
import { afterEach, beforeEach, describe, expect, jest, test } from '@jest/globals';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { ServiceNowClient } from '../src/servicenow-client.js';

const UPDATE_SET_SYS_ID = '0123456789abcdef0123456789abcdef';
const TRIGGER_SYS_ID = 'fedcba9876543210fedcba9876543210';
const SKIP_SYMLINKS = process.platform === 'win32';

// Each payload tries to break out of the context it is interpolated into.
// The `pwned` marker must never appear outside a serialized data literal.
const HOSTILE_VALUES = [
  ['single quote', "Fix'); gs.print('pwned-quote'); ('"],
  ['backslash before quote', "Fix\\'); gs.print('pwned-backslash'); //"],
  ['double quote', 'Fix" + gs.print("pwned-double") + "'],
  ['newline', "Fix\ngs.print('pwned-newline');"],
  ['carriage return', "Fix\rgs.print('pwned-cr');"],
  ['block comment terminator', "Fix */ gs.print('pwned-comment'); /*"],
  ['line separator', "Fix\u2028gs.print('pwned-ls');"],
  ['paragraph separator', "Fix\u2029gs.print('pwned-ps');"]
];

let tempRoot;
let projectDir;
let outsideDir;

beforeEach(async () => {
  tempRoot = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), 'happy-fix-script-')));
  projectDir = path.join(tempRoot, 'project');
  outsideDir = path.join(tempRoot, 'outside');
  await fs.mkdir(projectDir);
  await fs.mkdir(outsideDir);
  jest.spyOn(process, 'cwd').mockReturnValue(projectDir);
  jest.spyOn(console, 'error').mockImplementation(() => {});
  jest.spyOn(console, 'log').mockImplementation(() => {});
});

afterEach(async () => {
  jest.restoreAllMocks();
  await fs.rm(tempRoot, { recursive: true, force: true });
});

async function listFiles(dir) {
  const files = [];
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...await listFiles(full));
    } else {
      files.push(path.relative(tempRoot, full));
    }
  }
  return files.sort();
}

async function connectMcp(serviceNowClient) {
  const server = await createMcpServer(serviceNowClient, {
    configManager: { getInstance: jest.fn(), listInstances: jest.fn(() => []) },
    createServiceNowClient: jest.fn()
  });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'generated-script-safety', version: '1.0.0' });
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  return {
    callTool: (name, args) => client.callTool({ name, arguments: args }),
    close: async () => {
      await client.close();
      await server.close();
    }
  };
}

function offlineClient() {
  return {
    currentInstanceName: 'offline',
    setProgressCallback: jest.fn(),
    getCurrentInstance: jest.fn(() => ({ name: 'offline', url: 'http://127.0.0.1:9' }))
  };
}

async function startFakeServiceNow({ updateSetName, triggerStatus = 201 }) {
  const requests = [];
  const server = http.createServer((req, res) => {
    let raw = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => { raw += chunk; });
    req.on('end', () => {
      const url = new URL(req.url, 'http://127.0.0.1');
      requests.push({
        method: req.method,
        path: url.pathname,
        body: raw && (req.headers['content-type'] || '').includes('json') ? JSON.parse(raw) : null
      });
      const send = (status, payload) => {
        res.writeHead(status, { 'content-type': 'application/json' });
        res.end(JSON.stringify(payload));
      };

      if (req.method === 'GET' && url.pathname.startsWith('/api/now/table/sys_update_set/')) {
        // Answer any id so caller-supplied sys_ids reach script generation.
        return send(200, { result: { sys_id: UPDATE_SET_SYS_ID, name: updateSetName } });
      }
      if (req.method === 'GET' && url.pathname === '/') {
        res.writeHead(200, { 'content-type': 'text/html' });
        return res.end('<html></html>');
      }
      if (url.pathname === '/api/now/ui/concoursepicker/updateset') {
        return send(500, { error: { message: 'UI session required' } });
      }
      if (url.pathname.startsWith('/api/now/table/sys_trigger')) {
        if (triggerStatus >= 400) {
          return send(triggerStatus, { error: { message: 'sys_trigger write denied' } });
        }
        if (req.method === 'POST') {
          return send(201, { result: { sys_id: TRIGGER_SYS_ID, name: 'MCP_Script_1' } });
        }
        return send(200, { result: { sys_id: TRIGGER_SYS_ID, name: 'MCP_Script_1' } });
      }
      return send(404, { error: { message: `unexpected ${req.method} ${url.pathname}` } });
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    url: `http://127.0.0.1:${port}`,
    requests,
    close: () => new Promise((resolve) => server.close(resolve))
  };
}

function textOf(result) {
  return result.content.map((item) => item.text).join('\n');
}

/**
 * Proves `variable` is assigned exactly one JavaScript string/object literal
 * that decodes back to `expected`, and that no hostile marker survives
 * anywhere else in the generated source.
 */
function expectDataAssignment(source, variable, expected) {
  expect(() => new vm.Script(source)).not.toThrow();
  const prefix = `var ${variable} = `;
  // JavaScript line terminators: a literal that leaked one would span lines.
  const lines = source.split(/\r\n|[\n\r\u2028\u2029]/);
  const assignments = lines.filter((line) => line.startsWith(prefix));
  expect(assignments).toHaveLength(1);
  const literal = assignments[0].slice(prefix.length).replace(/;$/, '');
  // JSON.parse accepts exactly one complete literal, nothing appended.
  expect(JSON.parse(literal)).toEqual(expected);
  return lines.filter((line) => !line.startsWith(prefix)).join('\n');
}

function leadingBlockComment(source) {
  const start = source.indexOf('/*');
  return source.slice(start, source.indexOf('*/', start + 2) + 2);
}

describe('SN-Create-Fix-Script file containment (VULN-008)', () => {
  const INVALID_NAMES = [
    ['parent traversal', '../escape'],
    ['nested traversal', 'nested/../../escape'],
    ['bare dot-dot', '..'],
    ['bare dot', '.'],
    ['leading dot', '.hidden'],
    ['forward slash', 'sub/name'],
    ['backslash', 'sub\\name'],
    ['windows traversal', '..\\..\\escape'],
    ['absolute path', '<outside>/escape'],
    ['drive relative', 'C:escape'],
    ['drive absolute', 'C:\\escape'],
    ['UNC path', '\\\\server\\share\\escape'],
    ['NUL byte', 'name\u0000.txt'],
    ['newline', 'name\nmore'],
    ['DEL control', 'name\u007f'],
    ['empty', ''],
    ['too long', 'a'.repeat(101)],
    ['Windows device CON', 'CON'],
    ['Windows device with extension', 'con.x'],
    ['Windows device NUL', 'NUL.a'],
    ['Windows device PRN', 'Prn'],
    ['Windows device AUX', 'aux.backup'],
    ['Windows device COM', 'COM1'],
    ['Windows device LPT', 'lpt9.txt']
  ];

  test.each(INVALID_NAMES)('rejects %s before writing anything', async (_label, scriptName) => {
    const mcp = await connectMcp(offlineClient());
    try {
      const name = scriptName.replace('<outside>', outsideDir);
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: name,
        script_content: "gs.info('hello');"
      });
      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/Invalid script_name/);
      expect(await listFiles(tempRoot)).toEqual([]);
    } finally {
      await mcp.close();
    }
  });

  test('rejects a non-string script_name', async () => {
    const mcp = await connectMcp(offlineClient());
    try {
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: ['..', 'escape'],
        script_content: "gs.info('hello');"
      });
      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/Invalid script_name/);
      expect(await listFiles(tempRoot)).toEqual([]);
    } finally {
      await mcp.close();
    }
  });

  test('writes a valid name as a private direct child of the scripts directory', async () => {
    const mcp = await connectMcp(offlineClient());
    try {
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'link_ui_policy-actions.v2',
        script_content: "gs.info('linked');"
      });
      expect(result.isError).toBeFalsy();
      const files = await listFiles(tempRoot);
      expect(files).toHaveLength(1);
      expect(path.dirname(files[0])).toBe(path.join('project', 'scripts'));
      expect(path.basename(files[0])).toMatch(/^link_ui_policy-actions\.v2_\d{4}-\d{2}-\d{2}T[\d-]+Z\.js$/);
      const filePath = path.join(tempRoot, files[0]);
      expect(textOf(result)).toContain(filePath);
      const content = await fs.readFile(filePath, 'utf8');
      expect(content).toContain("gs.info('linked');");
      expect(() => new vm.Script(content)).not.toThrow();
      if (process.platform !== 'win32') {
        expect((await fs.stat(filePath)).mode & 0o777).toBe(0o600);
      }
    } finally {
      await mcp.close();
    }
  });

  (SKIP_SYMLINKS ? test.skip : test)('refuses a scripts directory that is a symlink', async () => {
    await fs.symlink(outsideDir, path.join(projectDir, 'scripts'), 'dir');
    const mcp = await connectMcp(offlineClient());
    try {
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'valid_name',
        script_content: "gs.info('hello');"
      });
      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/scripts directory/i);
      expect(await fs.readdir(outsideDir)).toEqual([]);
    } finally {
      await mcp.close();
    }
  });

  test.each([['console'], ['nullable'], ['com10'], ['lpt'], ['auxiliary.fix'], ['my_con']])(
    'accepts %s, which only resembles a Windows device name',
    async (scriptName) => {
      const mcp = await connectMcp(offlineClient());
      try {
        const result = await mcp.callTool('SN-Create-Fix-Script', {
          script_name: scriptName,
          script_content: "gs.info('hello');"
        });
        expect(result.isError).toBeFalsy();
        const files = await listFiles(tempRoot);
        expect(files).toHaveLength(1);
        expect(path.basename(files[0]).startsWith(`${scriptName}_`)).toBe(true);
      } finally {
        await mcp.close();
      }
    }
  );

  test('refuses a scripts path that is not a directory', async () => {
    await fs.writeFile(path.join(projectDir, 'scripts'), 'not a directory');
    const mcp = await connectMcp(offlineClient());
    try {
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'valid_name',
        script_content: "gs.info('hello');"
      });
      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/scripts directory .* is not a directory/);
      expect(await fs.readFile(path.join(projectDir, 'scripts'), 'utf8')).toBe('not a directory');
    } finally {
      await mcp.close();
    }
  });

  test('removes the partially written file when the write fails', async () => {
    const probe = await fs.open(path.join(tempRoot, 'probe'), 'w');
    const fileHandlePrototype = Object.getPrototypeOf(probe);
    await probe.close();
    await fs.rm(path.join(tempRoot, 'probe'));

    const mcp = await connectMcp(offlineClient());
    try {
      jest.spyOn(fileHandlePrototype, 'writeFile').mockRejectedValueOnce(
        Object.assign(new Error('simulated disk full'), { code: 'ENOSPC' })
      );
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'write_fails',
        script_content: "gs.info('hello');"
      });
      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/simulated disk full/);
      expect(await fs.readdir(path.join(projectDir, 'scripts'))).toEqual([]);
    } finally {
      await mcp.close();
    }
  });

  (SKIP_SYMLINKS ? test.skip : test)('never follows or replaces an existing target (symlink or file)', async () => {
    const fixedIso = '2026-10-06T01:02:03.004Z';
    const scriptsDir = path.join(projectDir, 'scripts');
    await fs.mkdir(scriptsDir);
    const victim = path.join(outsideDir, 'victim.txt');
    await fs.writeFile(victim, 'original');
    await fs.symlink(victim, path.join(scriptsDir, 'linked_2026-10-06T01-02-03-004Z.js'));
    await fs.writeFile(path.join(scriptsDir, 'existing_2026-10-06T01-02-03-004Z.js'), 'keep me');

    const mcp = await connectMcp(offlineClient());
    try {
      jest.spyOn(Date.prototype, 'toISOString').mockReturnValue(fixedIso);
      const viaSymlink = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'linked',
        script_content: "gs.info('overwrite');"
      });
      const viaFile = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'existing',
        script_content: "gs.info('overwrite');"
      });
      expect(viaSymlink.isError).toBe(true);
      expect(viaFile.isError).toBe(true);
      expect(textOf(viaSymlink)).toMatch(/already exists/);
      expect(await fs.readFile(victim, 'utf8')).toBe('original');
      expect(await fs.readFile(path.join(scriptsDir, 'existing_2026-10-06T01-02-03-004Z.js'), 'utf8')).toBe('keep me');
    } finally {
      await mcp.close();
    }
  });
});

describe('generated fix-script metadata stays out of executable code (VULN-010)', () => {
  test.each(HOSTILE_VALUES)('SN-Create-Fix-Script description with %s', async (_label, description) => {
    const mcp = await connectMcp(offlineClient());
    try {
      const result = await mcp.callTool('SN-Create-Fix-Script', {
        script_name: 'metadata_check',
        script_content: "gs.info('body');",
        description
      });
      expect(result.isError).toBeFalsy();
      const [file] = await listFiles(tempRoot);
      const content = await fs.readFile(path.join(tempRoot, file), 'utf8');

      expect(leadingBlockComment(content)).not.toContain('pwned');
      const remainder = expectDataAssignment(content, 'fixScriptMetadata', {
        script_name: 'metadata_check',
        description
      });
      expect(remainder).not.toContain('pwned');
      expect(remainder).toContain("gs.info('body');");
    } finally {
      await mcp.close();
    }
  });

  test.each(HOSTILE_VALUES)('SN-Execute-Background-Script fallback description with %s', async (_label, description) => {
    const fake = await startFakeServiceNow({ updateSetName: 'unused', triggerStatus: 403 });
    const mcp = await connectMcp(new ServiceNowClient(fake.url, 'fake-user', 'fake-password'));
    try {
      const result = await mcp.callTool('SN-Execute-Background-Script', {
        script: "gs.info('body');",
        description
      });
      expect(result.isError).toBeFalsy();
      const [file] = await listFiles(tempRoot);
      expect(path.dirname(file)).toBe(path.join('project', 'scripts'));
      const content = await fs.readFile(path.join(tempRoot, file), 'utf8');

      expect(leadingBlockComment(content)).not.toContain('pwned');
      const remainder = expectDataAssignment(content, 'fixScriptMetadata', { description });
      expect(remainder).not.toContain('pwned');
      expect(remainder).toContain("gs.info('body');");
    } finally {
      await mcp.close();
      await fake.close();
    }
  });
});

describe('update-set names are serialized as data (VULN-009)', () => {
  test.each(HOSTILE_VALUES)('sys_trigger fallback script with %s', async (_label, updateSetName) => {
    const fake = await startFakeServiceNow({ updateSetName });
    const mcp = await connectMcp(new ServiceNowClient(fake.url, 'fake-user', 'fake-password'));
    try {
      const result = await mcp.callTool('SN-Set-Update-Set', { update_set_sys_id: UPDATE_SET_SYS_ID });
      expect(result.isError).toBeFalsy();
      expect(textOf(result)).toMatch(/scheduled via sys_trigger/);

      const triggerPost = fake.requests.find((r) => r.method === 'POST' && r.path === '/api/now/table/sys_trigger');
      const script = triggerPost.body.script;
      expect(expectDataAssignment(script, 'updateSetId', UPDATE_SET_SYS_ID)).toBeDefined();
      const remainder = expectDataAssignment(script, 'updateSetName', updateSetName);
      expect(remainder).not.toContain('pwned');
      expect(remainder).toContain("gs.info('✅ Update set changed to: ' + updateSetName);");
      // Description is a sys_trigger field value (data), never script text.
      expect(triggerPost.body.description).toBe(`Set update set to: ${updateSetName}`);

      const wrapped = fake.requests.find((r) => r.method === 'PUT' && r.path === `/api/now/table/sys_trigger/${TRIGGER_SYS_ID}`);
      expect(() => new vm.Script(wrapped.body.script)).not.toThrow();
      expect(wrapped.body.script).toContain(script);
    } finally {
      await mcp.close();
      await fake.close();
    }
  });

  test.each(HOSTILE_VALUES)('manual fix-script file fallback with %s', async (_label, updateSetName) => {
    const fake = await startFakeServiceNow({ updateSetName, triggerStatus: 403 });
    const mcp = await connectMcp(new ServiceNowClient(fake.url, 'fake-user', 'fake-password'));
    try {
      const result = await mcp.callTool('SN-Set-Update-Set', { update_set_sys_id: UPDATE_SET_SYS_ID });
      expect(result.isError).toBeFalsy();
      expect(textOf(result)).toMatch(/Created fix script for manual execution/);

      const files = await listFiles(tempRoot);
      expect(files).toHaveLength(1);
      expect(path.dirname(files[0])).toBe(path.join('project', 'scripts'));
      expect(path.basename(files[0])).toMatch(/^set_update_set_[A-Za-z0-9_]+_[\dTZ-]+\.js$/);
      const content = await fs.readFile(path.join(tempRoot, files[0]), 'utf8');

      expect(leadingBlockComment(content)).not.toContain('pwned');
      expectDataAssignment(content, 'updateSetSysId', UPDATE_SET_SYS_ID);
      const remainder = expectDataAssignment(content, 'updateSetName', updateSetName);
      expect(remainder).not.toContain('pwned');
      expect(remainder).toContain('gus.set(updateSetSysId);');
      expect(remainder).toContain("gs.info('✅ Update set changed to: ' + updateSetName);");
    } finally {
      await mcp.close();
      await fake.close();
    }
  });

  test('bounds the file name derived from a very long update-set name', async () => {
    const fake = await startFakeServiceNow({ updateSetName: 'x'.repeat(400), triggerStatus: 403 });
    const mcp = await connectMcp(new ServiceNowClient(fake.url, 'fake-user', 'fake-password'));
    try {
      const result = await mcp.callTool('SN-Set-Update-Set', { update_set_sys_id: UPDATE_SET_SYS_ID });
      expect(result.isError).toBeFalsy();
      const files = await listFiles(tempRoot);
      expect(files).toHaveLength(1);
      expect(path.basename(files[0]).length).toBeLessThanOrEqual(120);
    } finally {
      await mcp.close();
      await fake.close();
    }
  });

  test.each(HOSTILE_VALUES)('caller-supplied update_set_sys_id with %s in both fallbacks', async (_label, sysId) => {
    const fake = await startFakeServiceNow({ updateSetName: 'Benign Name' });
    const mcp = await connectMcp(new ServiceNowClient(fake.url, 'fake-user', 'fake-password'));
    try {
      const scheduled = await mcp.callTool('SN-Set-Update-Set', { update_set_sys_id: sysId });
      expect(scheduled.isError).toBeFalsy();
      const triggerPost = fake.requests.find((r) => r.method === 'POST' && r.path === '/api/now/table/sys_trigger');
      const triggerRemainder = expectDataAssignment(triggerPost.body.script, 'updateSetId', sysId);
      expect(triggerRemainder).not.toContain('pwned');
      expect(triggerRemainder).toContain('gr.value = updateSetId;');
    } finally {
      await mcp.close();
      await fake.close();
    }

    const deniedFake = await startFakeServiceNow({ updateSetName: 'Benign Name', triggerStatus: 403 });
    const deniedMcp = await connectMcp(new ServiceNowClient(deniedFake.url, 'fake-user', 'fake-password'));
    try {
      const manual = await deniedMcp.callTool('SN-Set-Update-Set', { update_set_sys_id: sysId });
      expect(manual.isError).toBeFalsy();
      const [file] = await listFiles(tempRoot);
      const content = await fs.readFile(path.join(tempRoot, file), 'utf8');
      expect(leadingBlockComment(content)).not.toContain('pwned');
      const remainder = expectDataAssignment(content, 'updateSetSysId', sysId);
      expect(remainder).not.toContain('pwned');
      expect(remainder).toContain('gus.set(updateSetSysId);');
    } finally {
      await deniedMcp.close();
      await deniedFake.close();
    }
  });
});
