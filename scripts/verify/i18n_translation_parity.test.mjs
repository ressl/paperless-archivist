import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const checker = fileURLToPath(new URL('./i18n_translation_parity.mjs', import.meta.url));

function messagesSource(locales = ['en', 'de']) {
  return `export const enMessages = {
  'greeting': 'Hello {name}',
  'prompts.help.ocr.label': 'OCR',
} as const;

export type MessageKey = keyof typeof enMessages;
export type LocaleMessages = Record<MessageKey, string>;
export const completeLocales = ${JSON.stringify(locales)} as const;
`;
}

function localeSource(locale, entries) {
  const body = Object.entries(entries)
    .map(([key, value]) => `  '${key}': ${JSON.stringify(value)},`)
    .join('\n');
  return `import type { LocaleMessages } from '../messages';

const ${locale}Messages: LocaleMessages = {
${body}
};

export default ${locale}Messages;
`;
}

async function runFixture(t, { locales = ['en', 'de'], localeFiles = {} }) {
  const root = await mkdtemp(join(tmpdir(), 'archivist-i18n-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const messagesPath = join(root, 'frontend', 'src', 'i18n', 'messages.ts');
  await mkdir(dirname(messagesPath), { recursive: true });
  await mkdir(join(dirname(messagesPath), 'locales'), { recursive: true });
  await writeFile(messagesPath, messagesSource(locales));
  await Promise.all(
    Object.entries(localeFiles).map(([locale, source]) =>
      writeFile(join(dirname(messagesPath), 'locales', `${locale}.ts`), source)
    )
  );
  const result = spawnSync(process.execPath, [checker, '--root', root], {
    encoding: 'utf8'
  });
  return {
    ...result,
    output: `${result.stdout}\n${result.stderr}`
  };
}

test('valid inventory exits zero and documents an allowed identical technical term', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: localeSource('de', {
        greeting: 'Hallo // docs /* bleiben erhalten */ {name}',
        'prompts.help.ocr.label': 'OCR'
      })
    }
  });

  assert.equal(result.status, 0, result.output);
  assert.match(result.output, /Structural errors:\s+0/);
  assert.match(result.output, /\[identical-allowed\] locale=de key=prompts\.help\.ocr\.label/);
});

test('missing locale file exits non-zero with locale and error class', async (t) => {
  const result = await runFixture(t, { localeFiles: {} });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[missing-file\] locale=de/);
});

test('unparseable locale exits non-zero with locale and error class', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: "const deMessages = { 'greeting': 'Hallo {name}'"
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[parse-error\] locale=de/);
});

test('missing key exits non-zero with locale, key, and error class', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: localeSource('de', { 'prompts.help.ocr.label': 'OCR' })
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[missing-key\] locale=de key=greeting/);
});

test('commented-out key is treated as missing instead of parsed as a message', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: `import type { LocaleMessages } from '../messages';

const deMessages: LocaleMessages = {
  // 'greeting': 'Hallo {name}',
  'prompts.help.ocr.label': 'OCR',
};

export default deMessages;
`
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[missing-key\] locale=de key=greeting/);
});

test('key inside a block comment is treated as missing', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: `import type { LocaleMessages } from '../messages';

const deMessages: LocaleMessages = {
  /*
  'greeting': 'Hallo {name}',
  */
  'prompts.help.ocr.label': 'OCR',
};

export default deMessages;
`
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[missing-key\] locale=de key=greeting/);
});

test('syntax error after a message value exits non-zero as a parse error', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: `import type { LocaleMessages } from '../messages';

const deMessages: LocaleMessages = {
  'greeting': 'Hallo {name}' THIS_IS_NOT_TYPESCRIPT,
  'prompts.help.ocr.label': 'OCR',
};

export default deMessages;
`
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[parse-error\] locale=de/);
});

test('placeholder mismatch exits non-zero with locale, key, and error class', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: localeSource('de', {
        greeting: 'Hallo {benutzer}',
        'prompts.help.ocr.label': 'OCR'
      })
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[placeholder-mismatch\] locale=de key=greeting/);
  assert.match(result.output, /expected=name actual=benutzer/);
});

test('extra key exits non-zero with locale, key, and error class', async (t) => {
  const result = await runFixture(t, {
    localeFiles: {
      de: localeSource('de', {
        greeting: 'Hallo {name}',
        'prompts.help.ocr.label': 'OCR',
        unexpected: 'Unerwartet'
      })
    }
  });

  assert.notEqual(result.status, 0, result.output);
  assert.match(result.output, /\[extra-key\] locale=de key=unexpected/);
});
