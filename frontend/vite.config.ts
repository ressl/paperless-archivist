import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

type PackageManifest = {
  version?: string;
};

const frontendDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(frontendDir, '..');
const packageJson = JSON.parse(readFileSync(resolve(frontendDir, 'package.json'), 'utf8')) as PackageManifest;
const packageVersion = packageJson.version ?? '0.0.0';

function envValue(name: string): string | undefined {
  const value = process.env[name]?.trim();
  return value ? value : undefined;
}

function commandOutput(command: string): string | undefined {
  try {
    const output = execSync(command, {
      cwd: repoRoot,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore']
    }).trim();
    return output ? output : undefined;
  } catch {
    return undefined;
  }
}

function shortSha(value?: string): string {
  return value ? value.slice(0, 8) : '';
}

function githubTag(): string | undefined {
  if (envValue('GITHUB_REF_TYPE') === 'tag') {
    return envValue('GITHUB_REF_NAME');
  }

  const ref = envValue('GITHUB_REF');
  return ref?.startsWith('refs/tags/') ? ref.slice('refs/tags/'.length) : undefined;
}

function cleanLocalTag(): string | undefined {
  if (envValue('CI') === 'true') {
    return undefined;
  }

  const status = commandOutput('git status --porcelain');
  if (status) {
    return undefined;
  }

  return commandOutput('git describe --tags --exact-match HEAD');
}

const releaseTag = envValue('CI_COMMIT_TAG') ?? githubTag() ?? cleanLocalTag();
const commitSha = shortSha(envValue('CI_COMMIT_SHA') ?? envValue('GITHUB_SHA') ?? commandOutput('git rev-parse HEAD'));
const buildNumber = envValue('CI_PIPELINE_IID') ?? envValue('GITHUB_RUN_NUMBER') ?? '';
const appVersion = releaseTag ?? (commitSha ? `${packageVersion}+${commitSha}` : packageVersion);

export default defineConfig({
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(appVersion),
    __APP_COMMIT_SHA__: JSON.stringify(commitSha),
    __APP_BUILD_NUMBER__: JSON.stringify(buildNumber)
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/healthz': 'http://127.0.0.1:8080',
      '/readyz': 'http://127.0.0.1:8080'
    }
  },
  build: {
    sourcemap: false
  }
});
