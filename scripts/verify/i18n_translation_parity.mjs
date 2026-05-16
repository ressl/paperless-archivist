#!/usr/bin/env node
// Reports message keys whose non-English translation is identical to the
// English source, treated as "needs translation". The check is informational
// only: it always exits 0 so it can run in CI without gating merges. It also
// reports keys whose placeholder set (e.g. {value}, {provider}) differs from
// the English value, which catches genuine translation bugs that the
// identical-value check cannot. See issue #100.
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

// Discover which locales the file declares as complete. We look at the
// `completeLocales` tuple literal, e.g. `['en', 'de', 'fr']`, and parse out
// the locale tags. The English block is treated as the reference; we then
// look up each non-English locale.
const completeMatch = source.match(/completeLocales\s*=\s*\[([^\]]+)\]/);
if (!completeMatch) {
  console.error('i18n_translation_parity: could not locate completeLocales declaration');
  process.exit(0);
}
const locales = [...completeMatch[1].matchAll(/'([a-z-]+)'/g)].map((m) => m[1]);
if (locales.length === 0) {
  console.error('i18n_translation_parity: completeLocales is empty');
  process.exit(0);
}

// Find the boundaries for each `Messages` object literal. The convention in
// messages.ts is `export const <name>Messages = { ... } as const;` for English
// and `export const <name>Messages: Record<MessageKey, string> = { ... };`
// for the rest. Both blocks use the same `'key': 'value'` shape with no nested
// objects. A few values use double-quoted TS strings to avoid escaping inner
// apostrophes; we handle both quote styles.
function blockFor(name) {
  const marker = `export const ${name}Messages`;
  const start = source.indexOf(marker);
  if (start < 0) return null;
  // Capture up to the next `export const`/`export function`/`export type`
  // declaration, or to the end of the file.
  const tail = source.slice(start + marker.length);
  const next = tail.search(/\nexport (?:const|function|type) /);
  return next < 0 ? source.slice(start) : source.slice(start, start + marker.length + next);
}

const entryPattern = /'([^'\\]+)'\s*:\s*(?:'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)")/g;

function parseBlock(block) {
  const entries = new Map();
  if (!block) return entries;
  for (const match of block.matchAll(entryPattern)) {
    const key = match[1];
    if (entries.has(key)) continue;
    // match[2] = single-quoted value, match[3] = double-quoted value
    const value = match[2] !== undefined ? match[2] : match[3];
    entries.set(key, value);
  }
  return entries;
}

const enBlock = blockFor('en');
const en = parseBlock(enBlock);
if (en.size === 0) {
  console.error('i18n_translation_parity: failed to parse enMessages');
  process.exit(0);
}

function placeholdersOf(value) {
  const set = new Set();
  for (const match of value.matchAll(/\{(\w+)\}/g)) set.add(match[1]);
  return set;
}

function placeholderSetsEqual(a, b) {
  if (a.size !== b.size) return false;
  for (const item of a) if (!b.has(item)) return false;
  return true;
}

console.log('i18n translation parity report');
console.log('==============================');
console.log(`Reference locale (en) keys: ${en.size}`);
console.log(`Locales checked:            ${locales.filter((l) => l !== 'en').join(', ') || '(none)'}`);
console.log();

const summaryRows = [];
const detailSections = [];

for (const locale of locales) {
  if (locale === 'en') continue;
  const block = blockFor(locale);
  const parsed = parseBlock(block);
  const missing = [];
  const identical = [];
  const placeholderMismatch = [];
  for (const [key, enValue] of en) {
    const value = parsed.get(key);
    if (value === undefined) {
      missing.push(key);
      continue;
    }
    if (value === enValue) {
      identical.push({ key, value });
    }
    const enPlaceholders = placeholdersOf(enValue);
    const locPlaceholders = placeholdersOf(value);
    if (!placeholderSetsEqual(enPlaceholders, locPlaceholders)) {
      placeholderMismatch.push({ key, en: [...enPlaceholders], target: [...locPlaceholders] });
    }
  }
  const extras = [];
  for (const key of parsed.keys()) {
    if (!en.has(key)) extras.push(key);
  }
  summaryRows.push({
    locale,
    total: parsed.size,
    missing: missing.length,
    identical: identical.length,
    placeholderMismatch: placeholderMismatch.length,
    extras: extras.length
  });
  detailSections.push({ locale, missing, identical, placeholderMismatch, extras });
}

const pad = (s, n) => String(s).padEnd(n);
console.log(`${pad('Locale', 8)} ${pad('Keys', 6)} ${pad('Missing', 8)} ${pad('==EN', 6)} ${pad('PHmismatch', 11)} ${pad('Extras', 7)}`);
for (const row of summaryRows) {
  console.log(
    `${pad(row.locale, 8)} ${pad(row.total, 6)} ${pad(row.missing, 8)} ${pad(row.identical, 6)} ${pad(row.placeholderMismatch, 11)} ${pad(row.extras, 7)}`
  );
}

for (const section of detailSections) {
  if (
    section.missing.length === 0 &&
    section.identical.length === 0 &&
    section.placeholderMismatch.length === 0 &&
    section.extras.length === 0
  ) {
    continue;
  }
  console.log(`\n--- ${section.locale} ---`);
  if (section.missing.length > 0) {
    console.log(`Keys missing from ${section.locale}Messages:`);
    for (const key of section.missing) console.log(`  - ${key}`);
  }
  if (section.identical.length > 0) {
    console.log(`Keys with identical EN/${section.locale.toUpperCase()} values (likely untranslated or intentional loanwords):`);
    for (const { key, value } of section.identical) {
      const preview = value.length > 60 ? `${value.slice(0, 57)}...` : value;
      console.log(`  - ${key}  ->  ${preview}`);
    }
  }
  if (section.placeholderMismatch.length > 0) {
    console.log(`Keys with placeholder mismatch (placeholders must match the English value verbatim):`);
    for (const { key, en: enList, target } of section.placeholderMismatch) {
      console.log(`  - ${key}  en={${enList.join(',')}}  ${section.locale}={${target.join(',')}}`);
    }
  }
  if (section.extras.length > 0) {
    console.log(`Keys present in ${section.locale}Messages but not enMessages:`);
    for (const key of section.extras) console.log(`  - ${key}`);
  }
}

// Informational only; never fail. This script intentionally exits 0.
process.exit(0);
