#!/usr/bin/env node

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, normalize } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;

function collectMarkdown(dir) {
  const result = [];
  for (const entry of readdirSync(join(root, dir))) {
    const path = join(dir, entry);
    const full = join(root, path);
    const stat = statSync(full);
    if (stat.isDirectory()) {
      result.push(...collectMarkdown(path));
    } else if (entry.endsWith('.md')) {
      result.push(path);
    }
  }
  return result;
}

const files = [
  'README.md',
  'SECURITY.md',
  ...collectMarkdown('docs'),
  ...collectMarkdown('deploy'),
];

const failures = [];
const linkPattern = /\[[^\]]+\]\(([^)]+)\)/g;

for (const file of files) {
  const body = readFileSync(join(root, file), 'utf8');
  for (const match of body.matchAll(linkPattern)) {
    const rawTarget = match[1].trim();
    if (
      rawTarget.startsWith('http://') ||
      rawTarget.startsWith('https://') ||
      rawTarget.startsWith('mailto:') ||
      rawTarget.startsWith('#')
    ) {
      continue;
    }
    const withoutAnchor = rawTarget.split('#')[0];
    if (!withoutAnchor) continue;
    const target = normalize(join(root, dirname(file), withoutAnchor));
    if (!target.startsWith(root) || !existsSync(target)) {
      failures.push(`${file}: missing link target ${rawTarget}`);
    }
  }
}

if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exit(1);
}

console.log(`docs links ok: ${files.length} markdown files checked`);
