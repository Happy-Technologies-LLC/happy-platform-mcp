import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const GOLDEN_DIR = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
export const REPO = path.resolve(GOLDEN_DIR, '..', '..');
export const PRELOAD = path.join(GOLDEN_DIR, 'preload.mjs');
export const CLI_JS = path.join(REPO, 'src', 'cli.js');
export const STDIO_JS = path.join(REPO, 'src', 'stdio-server.js');
export const DEFAULT_GOLDEN_OUT = path.join(REPO, 'fixtures', 'golden');
