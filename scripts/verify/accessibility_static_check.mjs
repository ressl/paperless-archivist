#!/usr/bin/env node

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;
const app = readFileSync(join(root, 'frontend/src/App.tsx'), 'utf8');
const css = readFileSync(join(root, 'frontend/src/styles/app.css'), 'utf8');

const checks = [
  ['workspace main landmark', app.includes('<main className="workspace">')],
  ['login main landmark', app.includes('<main className="login">')],
  ['sidebar navigation landmark', app.includes('<nav>')],
  ['dashboard range group label', app.includes('aria-label={t(\'dashboard.range_label\')}')],
  ['workflow mode button group label', app.includes('aria-label={t(\'dashboard.auto.processing_mode\')}')],
  ['connection feedback live region', app.includes('aria-live="polite"')],
  ['model selects have accessible labels', app.includes('aria-label={`${provider.name} ${capability} model`}')],
  ['tooltip uses describedby', app.includes('aria-describedby={open ? tooltipId : undefined}')],
  ['tooltip supports escape close', app.includes("event.key === 'Escape'")],
  ['tooltip closes outside pointer/touch', app.includes("document.addEventListener('mousedown'") && app.includes("document.addEventListener('touchstart'")],
  ['global focus visible styles', css.includes('button:focus-visible') && css.includes('input:focus-visible') && css.includes('textarea:focus-visible')],
  ['icon reload button has aria-label', app.includes("aria-label={t('settings.ollama.reload_models')}")],
  ['user admin controls have labels', app.includes('aria-label="username"') && app.includes('aria-label="token name"')],
];

const failed = checks.filter(([, ok]) => !ok);

for (const [name, ok] of checks) {
  console.log(`${ok ? 'ok' : 'fail'} - ${name}`);
}

if (failed.length > 0) {
  console.error(`\nAccessibility static check failed: ${failed.length} issue(s).`);
  process.exit(1);
}
