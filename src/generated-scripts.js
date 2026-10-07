/**
 * Happy MCP Server - Generated script helpers
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 *
 * Untrusted values (update-set names, descriptions, caller-chosen names) are
 * embedded in generated ServiceNow JavaScript only as serialized data
 * literals, never as raw source or block-comment text. Fix-script files are
 * created only as new, direct children of the scripts directory.
 */

import { constants as fsConstants } from 'fs';
import fs from 'fs/promises';
import path from 'path';

const FIX_SCRIPT_NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/;
// Windows maps these names, with or without an extension, to devices.
const WINDOWS_DEVICE_NAME_PATTERN = /^(?:con|prn|aux|nul|com[0-9]|lpt[0-9])(?:\.|$)/i;
const FIX_SCRIPT_FILE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,199}\.js$/;
const DERIVED_NAME_MAX_LENGTH = 64;

/**
 * Serializes a value as a JavaScript literal. JSON escapes quotes,
 * backslashes and control characters; U+2028/U+2029 are additionally
 * escaped because pre-ES2019 engines treat them as line terminators.
 */
export function toJavaScriptLiteral(value) {
  return JSON.stringify(value)
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029');
}

/**
 * Validates a caller-supplied fix-script name: 1-100 characters of letters,
 * digits, '.', '_' or '-', starting with a letter or digit, and not a Windows
 * reserved device name. This excludes path separators, '.'/'..', drive or
 * absolute paths, control characters and device names.
 */
export function validateFixScriptName(name) {
  if (typeof name !== 'string' || !FIX_SCRIPT_NAME_PATTERN.test(name) || WINDOWS_DEVICE_NAME_PATTERN.test(name)) {
    throw new Error(
      'Invalid script_name: use 1-100 letters, digits, ".", "_" or "-", starting with a letter or digit ' +
      '(no path separators, "."/"..", drive or absolute paths, control characters, or Windows device names ' +
      'such as CON, NUL, COM1 or LPT1).'
    );
  }
  return name;
}

/** Reduces an untrusted value (e.g. an update-set name) to a bounded file-name fragment. */
export function toFileNameFragment(value) {
  const fragment = String(value ?? '').replace(/[^A-Za-z0-9]/g, '_').slice(0, DERIVED_NAME_MAX_LENGTH);
  return fragment || 'unnamed';
}

/** File-name-safe UTC timestamp, e.g. 2026-10-06T01-02-03-004Z. */
export function fileTimestamp(date = new Date()) {
  return date.toISOString().replace(/[:.]/g, '-');
}

/**
 * Builds a fix-script file. The header comment holds only fixed text;
 * untrusted values must reach the file only through `metadata` (emitted as a
 * `fixScriptMetadata` data literal) or a body built with toJavaScriptLiteral.
 */
export function buildFixScriptFileContent({ title, createdAt, notes = [], instructions, metadata, body }) {
  const lines = [
    '/**',
    ` * ${title}`,
    ` * Created: ${createdAt}`,
    ' *',
    ...notes.map((note) => (note ? ` * ${note}` : ' *')),
    ...(notes.length > 0 ? [' *'] : []),
    ' * Caller- or instance-supplied values appear only as data literals below,',
    ' * never as executable code or comment text.',
    ' *',
    ' * INSTRUCTIONS:',
    ...instructions.map((step, index) => ` * ${index + 1}. ${step}`),
    ' */',
    '',
    ...(metadata === undefined ? [] : [`var fixScriptMetadata = ${toJavaScriptLiteral(metadata)};`, '']),
    body,
    '',
    '// End of script',
    ''
  ];
  return lines.join('\n');
}

function scriptsDirectoryError(scriptsDir, reason) {
  return new Error(`Refusing to write fix script: scripts directory ${scriptsDir} ${reason}`);
}

/**
 * Creates `fileName` as a new private file directly inside `scriptsDir`.
 * Rejects non-basename names, a scripts directory that is a symlink or not a
 * directory, and any existing target (file or symlink) without following it.
 * Returns the absolute path of the written file.
 */
export async function writeFixScriptFile(scriptsDir, fileName, content) {
  if (typeof fileName !== 'string' || !FIX_SCRIPT_FILE_PATTERN.test(fileName)) {
    throw new Error('Refusing to write fix script: generated file name is not a safe basename');
  }

  const root = path.resolve(scriptsDir);
  await fs.mkdir(root, { recursive: true }).catch((error) => {
    // An existing non-directory or dangling link is reported by lstat below.
    if (error.code !== 'EEXIST') {
      throw error;
    }
  });
  const rootStat = await fs.lstat(root);
  if (rootStat.isSymbolicLink()) {
    throw scriptsDirectoryError(root, 'is a symbolic link');
  }
  if (!rootStat.isDirectory()) {
    throw scriptsDirectoryError(root, 'is not a directory');
  }

  const filePath = path.join(root, fileName);
  if (path.dirname(filePath) !== root || path.basename(filePath) !== fileName) {
    throw new Error('Refusing to write fix script: target is not a direct child of the scripts directory');
  }

  const flags = fsConstants.O_WRONLY | fsConstants.O_CREAT | fsConstants.O_EXCL | (fsConstants.O_NOFOLLOW ?? 0);
  let handle;
  try {
    handle = await fs.open(filePath, flags, 0o600);
  } catch (error) {
    if (error.code === 'EEXIST' || error.code === 'ELOOP') {
      throw new Error(`Refusing to write fix script: ${filePath} already exists`);
    }
    throw error;
  }

  try {
    await handle.writeFile(content, 'utf-8');
  } catch (error) {
    await handle.close();
    await fs.unlink(filePath).catch(() => {});
    throw error;
  }
  await handle.close();
  return filePath;
}
