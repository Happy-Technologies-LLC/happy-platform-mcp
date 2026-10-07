import fs from 'fs';
import os from 'os';
import path from 'path';
import { fileURLToPath } from 'url';
import { resolveConfigPaths } from '../config-path.js';

const DEFAULT_CACHE_DIR = path.join(os.homedir(), '.happy-platform-mcp', 'docs', 'servicenow');
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const DEFAULT_CONFIG_PATH = path.resolve(__dirname, '../../config/servicenow-instances.json');

function readSystemDocsProperties(env) {
  const configPath = resolveConfigPaths({ env, legacyPath: DEFAULT_CONFIG_PATH }).readPath;
  try {
    const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
    return config.docs || {};
  } catch (error) {
    if (error.code === 'ENOENT' || error instanceof SyntaxError) {
      return {};
    }
    throw new Error(`Failed to load docs system properties: ${error.message}`);
  }
}

function booleanProperty(value, defaultValue = false) {
  if (typeof value === 'boolean') return value;
  if (typeof value === 'string') return ['true', '1', 'yes'].includes(value.toLowerCase());
  return defaultValue;
}

function envHas(env, name) {
  return Object.prototype.hasOwnProperty.call(env, name);
}

export function getDocsConfig(env = process.env, systemProperties = readSystemDocsProperties(env)) {
  return {
    cacheDir: env.HAPPY_DOCS_CACHE_DIR || systemProperties.cacheDir || DEFAULT_CACHE_DIR,
    localIndexEnabled: envHas(env, 'HAPPY_DOCS_ENABLE_LOCAL_INDEX')
      ? booleanProperty(env.HAPPY_DOCS_ENABLE_LOCAL_INDEX)
      : booleanProperty(systemProperties.localIndexEnabled, false),
    enableVector: envHas(env, 'HAPPY_DOCS_ENABLE_VECTOR')
      ? booleanProperty(env.HAPPY_DOCS_ENABLE_VECTOR)
      : booleanProperty(systemProperties.enableVector, false),
    embeddingProvider: env.HAPPY_DOCS_EMBEDDING_PROVIDER || systemProperties.embeddingProvider || 'none'
  };
}

const NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/;
const SEGMENT_PATTERN = /^[A-Za-z0-9_~+=,@()-][A-Za-z0-9._~+=,@()-]{0,254}$/;
export const MAX_DOCS_PATH_LENGTH = 1024;
export const MAX_DOCS_PATH_DEPTH = 32;

function isSafeName(value) {
  return typeof value === 'string' && NAME_PATTERN.test(value) && !value.includes('..');
}

/** Family names and git refs share one conservative single-segment syntax. */
export function assertDocsFamilyName(family) {
  if (!isSafeName(family)) {
    throw new Error('Invalid ServiceNow docs family: expected 1-100 letters, digits, ".", "_" or "-"');
  }
  return family;
}

export function assertDocsBranchName(branch) {
  if (!isSafeName(branch)) {
    throw new Error('Invalid ServiceNow docs branch: expected 1-100 letters, digits, ".", "_" or "-"');
  }
  return branch;
}

/**
 * Canonical relative markdown path inside a docs branch. Rejects instead of
 * normalizing: no percent-encoding, dot segments, empty segments, separators
 * other than "/", controls, query/fragment markers, or drive/scheme colons.
 */
export function normalizeDocsDocumentPath(documentPath) {
  if (typeof documentPath !== 'string' || documentPath.length === 0) {
    throw new Error('Unsafe docs path: a relative markdown path is required');
  }
  if (documentPath.length > MAX_DOCS_PATH_LENGTH) {
    throw new Error(`Unsafe docs path: longer than ${MAX_DOCS_PATH_LENGTH} characters`);
  }
  const segments = documentPath.split('/');
  if (segments.length > MAX_DOCS_PATH_DEPTH) {
    throw new Error(`Unsafe docs path: deeper than ${MAX_DOCS_PATH_DEPTH} segments`);
  }
  if (!segments.every((segment) => SEGMENT_PATTERN.test(segment))) {
    throw new Error('Unsafe docs path: only relative paths of letters, digits and ._~+=,@()- segments are allowed');
  }
  if (!documentPath.endsWith('.md')) {
    throw new Error('Unsafe docs path: only .md documents are allowed');
  }
  return documentPath;
}

// Family markdown lives under its own subdirectory so no family name can
// address the index database (index.sqlite, -wal, -shm) in cacheDir.
export const DOCS_FILES_DIR = 'files';

export function resolveDocsCachePath(cacheDir, family, documentPath) {
  const parts = [assertDocsFamilyName(family)];
  if (documentPath !== undefined) {
    parts.push(...normalizeDocsDocumentPath(documentPath).split('/'));
  }
  const root = path.resolve(cacheDir, DOCS_FILES_DIR);
  const resolved = path.resolve(root, ...parts);

  if (!resolved.startsWith(root + path.sep)) {
    throw new Error('Unsafe docs path: resolves outside the docs cache');
  }

  return resolved;
}
