#!/usr/bin/env node
// Reports message keys whose German translation is identical to the English
// source, treated as "needs translation". The check is informational only:
// it always exits 0 so it can run in CI without gating merges. See issue #100.
//
// Usage:  node scripts/verify/i18n_translation_parity.mjs
//         pnpm i18n:check        (from frontend/)

import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..');
const messagesPath = resolve(repoRoot, 'frontend', 'src', 'i18n', 'messages.ts');

const source = await readFile(messagesPath, 'utf8');

// Find the two literal object boundaries. We rely on the fact that the file
// declares enMessages and deMessages each followed by a `{ ... } as const;`
// or `{ ... };` block. Both blocks use the same simple `'key': 'value'`
// shape — no nested objects, no escaped characters in keys.
const enStart = source.indexOf('export const enMessages');
const deStart = source.indexOf('export const deMessages');
if (enStart < 0 || deStart < 0) {
  console.error('i18n_translation_parity: could not locate enMessages/deMessages declarations');
  process.exit(0);
}

const enBlock = source.slice(enStart, deStart);
const deBlock = source.slice(deStart);

const entryPattern = /'([^'\\]+)'\s*:\s*'((?:[^'\\]|\\.)*)'/g;

function parseBlock(block) {
  const entries = new Map();
  for (const match of block.matchAll(entryPattern)) {
    const key = match[1];
    // Skip if this looks like a TypeScript type/declaration noise — the pattern
    // is a defensive guard; matches are already constrained by the regex.
    if (entries.has(key)) {
      // First definition wins; later collisions would be a bug elsewhere.
      continue;
    }
    entries.set(key, match[2]);
  }
  return entries;
}

const en = parseBlock(enBlock);
const de = parseBlock(deBlock);

const missing = [];
const identical = [];
for (const [key, enValue] of en) {
  const deValue = de.get(key);
  if (deValue === undefined) {
    missing.push(key);
    continue;
  }
  if (deValue === enValue) {
    identical.push({ key, value: enValue });
  }
}

const extras = [];
for (const key of de.keys()) {
  if (!en.has(key)) extras.push(key);
}

console.log('i18n translation parity report');
console.log('==============================');
console.log(`English keys:                ${en.size}`);
console.log(`German keys:                 ${de.size}`);
console.log(`Missing German translations: ${missing.length}`);
console.log(`Identical EN/DE values:      ${identical.length}`);
console.log(`German-only (no English):    ${extras.length}`);

if (missing.length > 0) {
  console.log('\nKeys missing from deMessages:');
  for (const key of missing) console.log(`  - ${key}`);
}

if (identical.length > 0) {
  console.log('\nKeys with identical EN/DE values (likely untranslated):');
  for (const { key, value } of identical) {
    const preview = value.length > 60 ? `${value.slice(0, 57)}...` : value;
    console.log(`  - ${key}  ->  ${preview}`);
  }
}

if (extras.length > 0) {
  console.log('\nKeys present in deMessages but not enMessages:');
  for (const key of extras) console.log(`  - ${key}`);
}

// Informational only; never fail. This script intentionally exits 0.
process.exit(0);
