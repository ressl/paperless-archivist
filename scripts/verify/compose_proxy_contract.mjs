#!/usr/bin/env node

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const requireFromFrontend = createRequire(join(repoRoot, 'frontend', 'package.json'));
const { parse } = requireFromFrontend('yaml');
const staticOnly = process.argv.includes('--static');

function read(relativePath) {
  return readFileSync(join(repoRoot, relativePath), 'utf8');
}

const exampleEnvironment = Object.fromEntries(
  read('deploy/compose/.env.example')
    .split(/\r?\n/)
    .filter((line) => line && !line.startsWith('#'))
    .map((line) => {
      const separator = line.indexOf('=');
      assert.notEqual(separator, -1, `invalid .env.example line: ${line}`);
      return [line.slice(0, separator), line.slice(separator + 1)];
    })
);
const composeEnvironment = { ...process.env, ...exampleEnvironment };

const baseCompose = parse(read('deploy/compose/docker-compose.yml'));
const proxyCompose = parse(read('deploy/compose/docker-compose.proxy.yml'));
const caddyfile = read('deploy/compose/Caddyfile');

assert.equal(
  baseCompose.services.api.environment.ARCHIVIST_COOKIE_SECURE,
  '${ARCHIVIST_COOKIE_SECURE:-false}',
  'the localhost HTTP profile must retain its explicit false default'
);
assert.ok(
  proxyCompose.services.api,
  'the reverse-proxy overlay must explicitly override the API service'
);
assert.equal(
  proxyCompose.services.api.environment.ARCHIVIST_COOKIE_SECURE,
  'true',
  'the reverse-proxy overlay must force secure session and CSRF cookies'
);
const hsts = caddyfile.match(
  /Strict-Transport-Security\s+"max-age=(\d+); includeSubDomains"/
);
assert.ok(hsts, 'the TLS virtual host must emit HSTS with includeSubDomains');
assert.ok(
  Number(hsts[1]) >= 31_536_000,
  'the TLS virtual host must retain HSTS for at least one year'
);
assert.match(
  caddyfile,
  /header_down\s+-Strict-Transport-Security/,
  'Caddy must replace the upstream HSTS value instead of returning duplicates'
);

if (!staticOnly) {
  function renderedCompose(extraFiles = [], profiles = []) {
    const args = [
      'compose',
      ...profiles.flatMap((profile) => ['--profile', profile]),
      '--env-file',
      join(repoRoot, 'deploy/compose/.env.example'),
      '-f',
      join(repoRoot, 'deploy/compose/docker-compose.yml'),
      ...extraFiles.flatMap((file) => ['-f', join(repoRoot, file)]),
      'config',
      '--format',
      'json'
    ];
    return JSON.parse(
      execFileSync('docker', args, {
        encoding: 'utf8',
        env: composeEnvironment
      })
    );
  }

  const local = renderedCompose();
  const proxy = renderedCompose(
    ['deploy/compose/docker-compose.proxy.yml'],
    ['reverse-proxy']
  );
  assert.equal(local.services.api.environment.ARCHIVIST_COOKIE_SECURE, 'false');
  assert.equal(proxy.services.api.environment.ARCHIVIST_COOKIE_SECURE, 'true');
  assert.ok(proxy.services.proxy, 'rendered proxy service is missing');
}

console.log(
  `Compose proxy contract valid (${staticOnly ? 'source' : 'source + rendered profiles'}).`
);
