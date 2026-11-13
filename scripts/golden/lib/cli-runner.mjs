// Runs `dispatch(argv, deps)` from src/cli.js in-process (child of the capture harness,
// started with the safety preload) with injected prompts that answer by prompt name.
// Registry = real InstanceRegistry on $HAPPY_CONFIG_PATH; credentials = real
// InstanceCredentialStore over the fake keyring. Prints one JSON document on stdout.
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const spec = JSON.parse(process.env.GOLDEN_CLI_SPEC);
const cli = await import(pathToFileURL(path.join(spec.repo, 'src', 'cli.js')).href);

const stdout = [];
const stderr = [];
const promptCalls = [];
const answer = (kind) => async (options) => {
  const configured = spec.answers?.[options.name];
  const value = configured !== undefined ? configured : options.default;
  promptCalls.push({ kind, name: options.name, message: options.message, hasMask: options.mask !== undefined, hasDefault: options.default !== undefined, answered: kind === 'password' ? '<masked>' : value });
  if (kind === 'password') return spec.secret ?? 'golden-fake-password-4f9a1c';
  return value;
};
const deps = {
  ...(spec.registryOptions ? { registryOptions: spec.registryOptions } : {}),
  stdout: { write: v => { stdout.push(String(v)); } },
  stderr: { write: v => { stderr.push(String(v)); } }
};
if (spec.prompts !== false) deps.prompts = { input: answer('input'), password: answer('password'), select: answer('select'), confirm: answer('confirm') };

let code;
try {
  code = await cli.dispatch(spec.argv, deps);
} catch (error) {
  code = `threw: ${error?.message}`;
}
process.stdout.write(JSON.stringify({ code, stdout: stdout.join(''), stderr: stderr.join(''), promptCalls }));
