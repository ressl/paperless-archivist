#!/usr/bin/env node

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;

// Concatenate every .tsx/.ts file under a feature dir so the checks survive the
// page being decomposed into sub-components (e.g. settings/ -> settings/sections/*,
// dashboard/ -> dashboard/*). We only care that a contract exists somewhere in
// the feature, not in one specific file.
const readTree = (rel) => {
  const dir = join(root, rel);
  let out = '';
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.isDirectory()) out += readTree(join(rel, entry.name));
    else if (/\.tsx?$/.test(entry.name)) out += readFileSync(join(dir, entry.name), 'utf8');
  }
  return out;
};

const app = readFileSync(join(root, 'frontend/src/App.tsx'), 'utf8');
const dashboard = readTree('frontend/src/dashboard');
const ui = readFileSync(join(root, 'frontend/src/lib/ui.tsx'), 'utf8');
const users = readFileSync(join(root, 'frontend/src/users/Users.tsx'), 'utf8');
const prompts = readFileSync(join(root, 'frontend/src/prompts/Prompts.tsx'), 'utf8');
const settings = readTree('frontend/src/settings');
const readCssTree = (rel) => {
  const dir = join(root, rel);
  let out = '';
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    if (entry.isDirectory()) out += readCssTree(join(rel, entry.name));
    else if (entry.name.endsWith('.css')) out += readFileSync(join(dir, entry.name), 'utf8');
  }
  return out;
};
const css = readCssTree('frontend/src/styles');

// --- Colour contrast (WCAG 2.x AA, #430) ------------------------------------
// Resolve the design tokens declared in the first `:root { ... }` block and
// check that every text colour token reaches 4.5:1 against every background it
// is used on. Adding a lighter --muted (or a darker surface) fails the check.
const rootBlock = css.match(/:root\s*\{([\s\S]*?)\n\}/)?.[1] ?? '';
const tokens = Object.fromEntries(
  [...rootBlock.matchAll(/--([\w-]+):\s*(#[0-9a-fA-F]{6})\b/g)].map(([, name, value]) => [name, value.toLowerCase()])
);
const luminance = (hex) => {
  const [r, g, b] = [1, 3, 5]
    .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const contrast = (a, b) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};
const TEXT_TOKENS = ['text', 'ink', 'ink-2', 'muted'];
const BACKGROUND_TOKENS = ['base', 'surface', 'surface-2'];
// Text on filled accent controls (primary buttons, active tabs, badges). #450
const PAIRS_BOTH_THEMES = [['on-accent', 'accent-bg'], ['on-accent', 'danger-solid']];
// Tone text on its soft fill. Checked for the dark theme, which #450 added;
// some light-theme tone pairs predate the check (tracked separately).
const PAIRS_DARK_ONLY = [
  ['teal', 'teal-soft'], ['teal-fg', 'teal-soft'], ['danger', 'danger-soft'], ['info', 'info-soft'],
  ['brass', 'brass-soft'], ['warning-text', 'warning-bg'], ['warning-fg', 'warning-strong'],
  ['conflict-fg', 'conflict-bg'], ['on-accent', 'brass-solid'], ['info', 'surface'], ['teal', 'surface'],
  ['danger', 'surface'], ['muted', 'teal-soft'], ['muted', 'info-soft'], ['muted', 'danger-soft']
];
const contrastFailures = [];
const checkPair = (palette, theme, fg, bg) => {
  if (!palette[fg] || !palette[bg]) {
    contrastFailures.push(`${theme}: --${fg}/--${bg} token missing`);
    return;
  }
  const ratio = contrast(palette[fg], palette[bg]);
  if (ratio < 4.5) contrastFailures.push(`${theme}: --${fg} on --${bg} is ${ratio.toFixed(2)}:1`);
};

// Dark theme (#450): the explicit `:root[data-theme='dark']` block and the
// `prefers-color-scheme` block must define identical tokens, must redefine
// every light colour token (a forgotten token would render light-on-dark),
// and must pass the same contrast bar.
const parseTokens = (block) =>
  Object.fromEntries(
    [...block.matchAll(/--([\w-]+):\s*(#[0-9a-fA-F]{6})\b/g)].map(([, name, value]) => [name, value.toLowerCase()])
  );
const explicitDarkBlock = css.match(/:root\[data-theme='dark'\]\s*\{([\s\S]*?)\n\}/)?.[1] ?? '';
const mediaDarkBlock =
  css.match(/@media \(prefers-color-scheme: dark\)\s*\{\s*:root:not\(\[data-theme='light'\]\)\s*\{([\s\S]*?)\n {2}\}/)?.[1] ?? '';
const explicitDark = parseTokens(explicitDarkBlock);
const mediaDark = parseTokens(mediaDarkBlock);
const darkThemeFailures = [];
if (Object.keys(explicitDark).length === 0) darkThemeFailures.push("no :root[data-theme='dark'] token block");
if (Object.keys(mediaDark).length === 0) darkThemeFailures.push('no prefers-color-scheme dark token block');
if (JSON.stringify(Object.entries(explicitDark).sort()) !== JSON.stringify(Object.entries(mediaDark).sort())) {
  darkThemeFailures.push('explicit and prefers-color-scheme dark tokens differ');
}
for (const name of Object.keys(tokens)) {
  if (!explicitDark[name]) darkThemeFailures.push(`--${name} has no dark value`);
}
if (!explicitDarkBlock.includes('color-scheme: dark')) darkThemeFailures.push('dark block must set color-scheme: dark');
if (darkThemeFailures.length) console.error(`dark theme: ${darkThemeFailures.join('; ')}`);
const darkTokens = { ...tokens, ...explicitDark };

for (const [theme, palette] of [['light', tokens], ['dark', darkTokens]]) {
  for (const fg of TEXT_TOKENS) {
    for (const bg of BACKGROUND_TOKENS) checkPair(palette, theme, fg, bg);
  }
  for (const [fg, bg] of PAIRS_BOTH_THEMES) checkPair(palette, theme, fg, bg);
}
for (const [fg, bg] of PAIRS_DARK_ONLY) checkPair(darkTokens, 'dark', fg, bg);
if (contrastFailures.length) console.error(`contrast: ${contrastFailures.join('; ')}`);

const inAnySource = (needle) =>
  app.includes(needle)
  || dashboard.includes(needle)
  || ui.includes(needle)
  || users.includes(needle)
  || prompts.includes(needle)
  || settings.includes(needle);

const checks = [
  ['workspace main landmark is the skip-link target', app.includes('<main className="workspace" id={MAIN_CONTENT_ID} tabIndex={-1}>')],
  ['login main landmark', app.includes('<main className="login">')],
  ['sidebar navigation landmark is labelled', app.includes("<nav aria-label={t('nav.main_label')}>")],
  // #431: skip link, current-page marker and collapsible mobile navigation.
  ['skip link to main content', app.includes('className="skip-link" href={`#${MAIN_CONTENT_ID}`}') && css.includes('.skip-link:focus')],
  ['active nav entry sets aria-current', app.includes("aria-current={active ? 'page' : undefined}")],
  ['mobile nav toggle exposes expanded state', app.includes('aria-expanded={menuOpen}') && app.includes('aria-controls={SIDEBAR_PANEL_ID}') && css.includes('.sidebar--open .sidebar-panel')],
  ['dashboard range group label', inAnySource("aria-label={t('dashboard.range_label')}")],
  ['workflow mode button group label', inAnySource("aria-label={t('dashboard.auto.processing_mode')}")],
  ['dashboard tablist has role and label', dashboard.includes('role="tablist"') && dashboard.includes("aria-label={t('dashboard.title')}")],
  // Status badges are rendered by the hundred (inventory rows, debug console
  // polling); they must stay plain text, not live regions. (#425)
  ['status pills are not live regions', ui.includes('<span className={`status ${tone}`}>')],
  ['connection feedback live region', inAnySource('aria-live="polite"')],
  ['model selects have accessible labels', settings.includes('aria-label={`${provider.name} ${capability} model`}')],
  ['tooltip uses describedby', inAnySource('aria-describedby={open ? tooltipId : undefined}')],
  ['tooltip supports escape close', inAnySource("event.key === 'Escape'")],
  ['tooltip closes outside pointer/touch', inAnySource("document.addEventListener('mousedown'") && inAnySource("document.addEventListener('touchstart'")],
  ['global focus visible styles', css.includes('button:focus-visible') && css.includes('input:focus-visible') && css.includes('textarea:focus-visible')],
  ['icon reload button has aria-label', settings.includes("aria-label={t('settings.ollama.reload_models')}")],
  ['user admin controls have labels', users.includes("aria-label={t('auth.username')}") && users.includes("aria-label={t('auth.password')}")],
  ['prefers-reduced-motion respected', css.includes('@media (prefers-reduced-motion: reduce)')],
  ['text colour tokens reach WCAG AA 4.5:1 on base/surface backgrounds (light and dark)', contrastFailures.length === 0],
  // #450: dark mode follows prefers-color-scheme and the per-browser toggle.
  ['dark theme tokens are complete and identical for the toggle and prefers-color-scheme', darkThemeFailures.length === 0],
  ['theme toggle is labelled', app.includes('<ThemeSelector />') && readFileSync(join(root, 'frontend/src/lib/theme.tsx'), 'utf8').includes("aria-label={t('theme.label')}")],
  // #450: notifications are a queued toast stack with always-mounted live regions.
  ['toast stack has assertive and polite live regions', (() => {
    const toast = readFileSync(join(root, 'frontend/src/lib/toast.tsx'), 'utf8');
    return toast.includes('role="alert" aria-live="assertive"') && toast.includes('role="status" aria-live="polite"') && app.includes('<ToastProvider>');
  })()],
];

const failed = checks.filter(([, ok]) => !ok);

for (const [name, ok] of checks) {
  console.log(`${ok ? 'ok' : 'fail'} - ${name}`);
}

if (failed.length > 0) {
  console.error(`\nAccessibility static check failed: ${failed.length} issue(s).`);
  process.exit(1);
}
