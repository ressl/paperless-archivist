#!/usr/bin/env node
// Validates the structural parity of every complete UI locale against English.
// Missing/unparseable files, missing or extra keys, and placeholder drift are
// merge-blocking errors. Identical values remain non-blocking language-quality
// warnings; well-known product names and technical abbreviations are documented
// explicitly below.
//
// Usage:  node scripts/verify/i18n_translation_parity.mjs
//         node scripts/verify/i18n_translation_parity.mjs --root <fixture-root>
//         pnpm i18n:check        (from frontend/)

import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const toolingRepoRoot = resolve(here, '..', '..');
const require = createRequire(resolve(toolingRepoRoot, 'frontend', 'package.json'));
const ts = require('typescript');
const rootArgument = process.argv.indexOf('--root');
const repoRoot =
  rootArgument >= 0 && process.argv[rootArgument + 1]
    ? resolve(process.argv[rootArgument + 1])
    : resolve(here, '..', '..');
const messagesPath = resolve(repoRoot, 'frontend', 'src', 'i18n', 'messages.ts');
const localesDir = resolve(repoRoot, 'frontend', 'src', 'i18n', 'locales');

const identicalAllowlist = new Map([
  ['app.name', 'product name'],
  ['app.loading', 'product name'],
  ['inventory.ocr', 'standard technical abbreviation'],
  ['stage.ocr', 'standard technical abbreviation'],
  ['prompts.help.ocr.label', 'standard technical abbreviation'],
  ['prompts.help.ocr.short_label', 'standard technical abbreviation']
]);

const structuralErrors = [];
const summaries = [];
const identicalValues = [];

function structuralError(kind, locale, key, detail) {
  structuralErrors.push({ kind, locale, key, detail });
}

async function readSource(path, locale) {
  try {
    return await readFile(path, 'utf8');
  } catch (error) {
    structuralError('missing-file', locale, null, `${path}: ${error.code ?? error.message}`);
    return null;
  }
}

function parseSourceFile(source, path, locale) {
  const sourceFile = ts.createSourceFile(path, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  if (sourceFile.parseDiagnostics.length === 0) return sourceFile;

  for (const diagnostic of sourceFile.parseDiagnostics) {
    const message = ts.flattenDiagnosticMessageText(diagnostic.messageText, ' ');
    const position =
      diagnostic.start === undefined
        ? path
        : (() => {
            const { line, character } = sourceFile.getLineAndCharacterOfPosition(diagnostic.start);
            return `${path}:${line + 1}:${character + 1}`;
          })();
    structuralError('parse-error', locale, null, `${position} ${message}`);
  }
  return null;
}

function unwrapExpression(expression) {
  let current = expression;
  while (
    current &&
    (ts.isAsExpression(current) ||
      ts.isSatisfiesExpression(current) ||
      ts.isParenthesizedExpression(current) ||
      ts.isTypeAssertionExpression(current))
  ) {
    current = current.expression;
  }
  return current;
}

function variableDeclaration(sourceFile, name) {
  for (const statement of sourceFile.statements) {
    if (!ts.isVariableStatement(statement)) continue;
    for (const declaration of statement.declarationList.declarations) {
      if (ts.isIdentifier(declaration.name) && declaration.name.text === name) return declaration;
    }
  }
  return null;
}

function localeListFrom(sourceFile) {
  const declaration = variableDeclaration(sourceFile, 'completeLocales');
  if (!declaration?.initializer) {
    structuralError('parse-error', 'en', 'completeLocales', 'declaration not found');
    return [];
  }

  const initializer = unwrapExpression(declaration.initializer);
  if (!initializer || !ts.isArrayLiteralExpression(initializer)) {
    structuralError('parse-error', 'en', 'completeLocales', 'must be an array literal');
    return [];
  }

  const locales = [];
  for (const element of initializer.elements) {
    if (!ts.isStringLiteralLike(element)) {
      structuralError('parse-error', 'en', 'completeLocales', 'entries must be string literals');
      continue;
    }
    locales.push(element.text);
  }
  if (locales.length === 0) {
    structuralError('parse-error', 'en', 'completeLocales', 'declaration is empty');
  }
  if (!locales.includes('en')) {
    structuralError('parse-error', 'en', 'completeLocales', 'reference locale en is missing');
  }
  return locales;
}

function parseMessages(sourceFile, locale) {
  const variableName = `${locale}Messages`;
  const declaration = variableDeclaration(sourceFile, variableName);
  const initializer = declaration?.initializer ? unwrapExpression(declaration.initializer) : null;
  if (!initializer || !ts.isObjectLiteralExpression(initializer)) {
    structuralError('parse-error', locale, null, `${variableName} object declaration not found`);
    return null;
  }

  if (locale !== 'en') {
    const hasExpectedDefaultExport = sourceFile.statements.some(
      (statement) =>
        ts.isExportAssignment(statement) &&
        !statement.isExportEquals &&
        ts.isIdentifier(statement.expression) &&
        statement.expression.text === variableName
    );
    if (!hasExpectedDefaultExport) {
      structuralError('parse-error', locale, null, `export default ${variableName} not found`);
    }
  }

  const entries = new Map();
  for (const property of initializer.properties) {
    if (!ts.isPropertyAssignment(property) || !ts.isStringLiteralLike(property.name)) {
      structuralError('parse-error', locale, null, 'message entries must be quoted property assignments');
      continue;
    }
    const key = property.name.text;
    if (entries.has(key)) {
      structuralError('parse-error', locale, key, 'duplicate message key');
      continue;
    }
    const value = unwrapExpression(property.initializer);
    if (!value || !ts.isStringLiteralLike(value)) {
      structuralError('parse-error', locale, key, 'message value must be a string literal');
      continue;
    }
    entries.set(key, value.text);
  }
  if (entries.size === 0) {
    structuralError('parse-error', locale, null, 'no message entries parsed');
    return null;
  }
  return entries;
}

function placeholdersOf(value) {
  const placeholders = new Set();
  for (const match of value.matchAll(/\{(\w+)\}/g)) placeholders.add(match[1]);
  return placeholders;
}

function placeholderSetsEqual(left, right) {
  if (left.size !== right.size) return false;
  for (const item of left) {
    if (!right.has(item)) return false;
  }
  return true;
}

const source = await readSource(messagesPath, 'en');
let locales = [];
let english = null;
if (source) {
  const sourceFile = parseSourceFile(source, messagesPath, 'en');
  if (sourceFile) {
    locales = localeListFrom(sourceFile);
    english = parseMessages(sourceFile, 'en');
  }
}

if (english) {
  for (const locale of locales) {
    if (locale === 'en') continue;
    const localePath = resolve(localesDir, `${locale}.ts`);
    const localeSource = await readSource(localePath, locale);
    const localeSourceFile = localeSource ? parseSourceFile(localeSource, localePath, locale) : null;
    const parsed = localeSourceFile ? parseMessages(localeSourceFile, locale) : null;
    if (!parsed) {
      summaries.push({ locale, total: 0, missing: english.size, identical: 0, placeholders: 0, extras: 0 });
      continue;
    }

    let missing = 0;
    let identical = 0;
    let placeholders = 0;
    for (const [key, referenceValue] of english) {
      const translatedValue = parsed.get(key);
      if (translatedValue === undefined) {
        missing += 1;
        structuralError('missing-key', locale, key, 'translation key is absent');
        continue;
      }
      if (translatedValue === referenceValue) {
        identical += 1;
        identicalValues.push({ locale, key, value: translatedValue });
      }
      const expected = placeholdersOf(referenceValue);
      const actual = placeholdersOf(translatedValue);
      if (!placeholderSetsEqual(expected, actual)) {
        placeholders += 1;
        structuralError(
          'placeholder-mismatch',
          locale,
          key,
          `expected=${[...expected].sort().join(',')} actual=${[...actual].sort().join(',')}`
        );
      }
    }

    let extras = 0;
    for (const key of parsed.keys()) {
      if (english.has(key)) continue;
      extras += 1;
      structuralError('extra-key', locale, key, 'key is absent from the English reference');
    }
    summaries.push({ locale, total: parsed.size, missing, identical, placeholders, extras });
  }
}

const pad = (value, length) => String(value).padEnd(length);
console.log('i18n translation parity report');
console.log('==============================');
console.log(`Reference locale (en) keys: ${english?.size ?? 0}`);
console.log(`Locales checked:            ${locales.filter((locale) => locale !== 'en').join(', ') || '(none)'}`);
console.log();
console.log(
  `${pad('Locale', 8)} ${pad('Keys', 6)} ${pad('Missing', 8)} ${pad('==EN', 6)} ${pad('PHmismatch', 11)} ${pad('Extras', 7)}`
);
for (const summary of summaries) {
  console.log(
    `${pad(summary.locale, 8)} ${pad(summary.total, 6)} ${pad(summary.missing, 8)} ${pad(summary.identical, 6)} ${pad(summary.placeholders, 11)} ${pad(summary.extras, 7)}`
  );
}

if (identicalValues.length > 0) {
  console.log('\nIdentical values (non-blocking):');
  for (const { locale, key, value } of identicalValues) {
    const reason = identicalAllowlist.get(key);
    if (reason) {
      console.log(`[identical-allowed] locale=${locale} key=${key} reason=${reason}`);
    } else {
      const preview = value.length > 60 ? `${value.slice(0, 57)}...` : value;
      console.log(`[identical-warning] locale=${locale} key=${key} value=${preview}`);
    }
  }
}

if (structuralErrors.length > 0) {
  console.error('\nStructural diagnostics:');
  for (const { kind, locale, key, detail } of structuralErrors) {
    console.error(`[${kind}] locale=${locale}${key ? ` key=${key}` : ''} ${detail}`);
  }
}

console.log(`\nStructural errors: ${structuralErrors.length}`);
process.exitCode = structuralErrors.length > 0 ? 1 : 0;
