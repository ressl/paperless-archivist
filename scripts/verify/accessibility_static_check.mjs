#!/usr/bin/env node

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;
const app = readFileSync(join(root, 'frontend/src/App.tsx'), 'utf8');
const dashboard = readFileSync(join(root, 'frontend/src/dashboard/Dashboard.tsx'), 'utf8');
const ui = readFileSync(join(root, 'frontend/src/lib/ui.tsx'), 'utf8');
const users = readFileSync(join(root, 'frontend/src/users/Users.tsx'), 'utf8');
const prompts = readFileSync(join(root, 'frontend/src/prompts/Prompts.tsx'), 'utf8');
const settings = readFileSync(join(root, 'frontend/src/settings/SettingsPage.tsx'), 'utf8');
const css = readFileSync(join(root, 'frontend/src/styles/app.css'), 'utf8');
const inAnySource = (needle) =>
  app.includes(needle)
  || dashboard.includes(needle)
  || ui.includes(needle)
  || users.includes(needle)
  || prompts.includes(needle)
  || settings.includes(needle);

const checks = [
  ['workspace main landmark', app.includes('<main className="workspace">')],
  ['login main landmark', app.includes('<main className="login">')],
  ['sidebar navigation landmark', app.includes('<nav>')],
  ['dashboard range group label', inAnySource("aria-label={t('dashboard.range_label')}")],
  ['workflow mode button group label', inAnySource("aria-label={t('dashboard.auto.processing_mode')}")],
  ['dashboard tablist has role and label', dashboard.includes('role="tablist"') && dashboard.includes("aria-label={t('dashboard.title')}")],
  ['status pills expose aria-label', ui.includes('role="status" aria-label={label}')],
  ['connection feedback live region', inAnySource('aria-live="polite"')],
  ['model selects have accessible labels', settings.includes('aria-label={`${provider.name} ${capability} model`}')],
  ['tooltip uses describedby', inAnySource('aria-describedby={open ? tooltipId : undefined}')],
  ['tooltip supports escape close', inAnySource("event.key === 'Escape'")],
  ['tooltip closes outside pointer/touch', inAnySource("document.addEventListener('mousedown'") && inAnySource("document.addEventListener('touchstart'")],
  ['global focus visible styles', css.includes('button:focus-visible') && css.includes('input:focus-visible') && css.includes('textarea:focus-visible')],
  ['icon reload button has aria-label', settings.includes("aria-label={t('settings.ollama.reload_models')}")],
  ['user admin controls have labels', users.includes("aria-label={t('auth.username')}") && users.includes("aria-label={t('auth.password')}")],
  ['prefers-reduced-motion respected', css.includes('@media (prefers-reduced-motion: reduce)')],
];

const failed = checks.filter(([, ok]) => !ok);

for (const [name, ok] of checks) {
  console.log(`${ok ? 'ok' : 'fail'} - ${name}`);
}

if (failed.length > 0) {
  console.error(`\nAccessibility static check failed: ${failed.length} issue(s).`);
  process.exit(1);
}
