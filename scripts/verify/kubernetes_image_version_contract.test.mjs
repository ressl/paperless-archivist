import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const requireFromFrontend = createRequire(
  new URL('../../frontend/package.json', import.meta.url)
);
const { parse } = requireFromFrontend('yaml');
const repositoryRoot = new URL('../../', import.meta.url);

async function readText(relativePath) {
  return readFile(new URL(relativePath, repositoryRoot), 'utf8');
}

async function workspaceVersion() {
  const cargo = await readText('Cargo.toml');
  const section = cargo.split('[workspace.package]')[1] ?? '';
  const match = section.match(/^version\s*=\s*"([^"]+)"/m);
  assert.ok(match, 'Cargo.toml [workspace.package] declares a version');
  return match[1];
}

// #436: the base manifests used to pin a stale `0.1.0` placeholder image.
test('kustomize base resolves the logical image to the release version', async () => {
  const version = await workspaceVersion();
  const kustomization = parse(await readText('deploy/kubernetes/base/kustomization.yaml'));
  const image = (kustomization.images ?? []).find((entry) => entry.name === 'paperless-archivist');
  assert.ok(image, 'kustomization.yaml has an images entry for paperless-archivist');
  assert.equal(image.newTag, version);

  for (const manifest of ['deployment-api.yaml', 'deployment-worker.yaml']) {
    const deployment = parse(await readText(`deploy/kubernetes/base/${manifest}`));
    for (const container of deployment.spec.template.spec.containers) {
      assert.equal(container.image, 'paperless-archivist', `${manifest} uses the logical image name`);
    }
  }
});

test('values example tracks the release version', async () => {
  const values = parse(await readText('deploy/kubernetes/values.example.yaml'));
  assert.equal(values.image.tag, await workspaceVersion());
});
