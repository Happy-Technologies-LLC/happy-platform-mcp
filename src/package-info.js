import { readFileSync } from 'node:fs';

const packageInfo = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));

export const PACKAGE_NAME = packageInfo.name;
export const PACKAGE_VERSION = packageInfo.version;
