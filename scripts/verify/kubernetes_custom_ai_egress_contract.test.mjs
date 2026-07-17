import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const requireFromFrontend = createRequire(
  new URL('../../frontend/package.json', import.meta.url)
);
const { parse, parseAllDocuments } = requireFromFrontend('yaml');
const repositoryRoot = new URL('../../', import.meta.url);

async function loadYaml(path) {
  return parse(await readFile(new URL(path, repositoryRoot), 'utf8'));
}

function egressPorts(policy) {
  return (policy.spec?.egress ?? []).flatMap((rule) => rule.ports ?? []);
}

test('restrictive base does not gain custom AI example ports', async () => {
  const base = await loadYaml('deploy/kubernetes/base/networkpolicy.yaml');
  assert.equal(base.kind, 'NetworkPolicy');
  assert.equal(base.metadata?.name, 'paperless-archivist');
  const ports = egressPorts(base).map(({ port }) => port);
  assert.doesNotMatch(JSON.stringify(base), /custom-ai|sglang|mineru/i);
  assert.ok(!ports.includes(30000));
  assert.ok(!ports.includes(8001));
});

test('component adds only the custom AI NetworkPolicy resource', async () => {
  const component = await loadYaml(
    'deploy/kubernetes/components/custom-ai-egress/kustomization.yaml'
  );
  assert.equal(component.apiVersion, 'kustomize.config.k8s.io/v1alpha1');
  assert.equal(component.kind, 'Component');
  assert.deepEqual(component.resources, ['networkpolicy.yaml']);
});

test('custom AI peers are namespace, pod and TCP-port constrained for API and worker', async () => {
  const policy = await loadYaml(
    'deploy/kubernetes/components/custom-ai-egress/networkpolicy.yaml'
  );
  assert.equal(policy.apiVersion, 'networking.k8s.io/v1');
  assert.equal(policy.kind, 'NetworkPolicy');
  assert.equal(policy.metadata?.name, 'paperless-archivist-custom-ai-egress');
  assert.deepEqual(policy.spec?.policyTypes, ['Egress']);
  assert.equal(policy.spec?.ingress, undefined);
  assert.deepEqual(policy.spec?.podSelector?.matchLabels, {
    'app.kubernetes.io/name': 'paperless-archivist'
  });
  assert.deepEqual(policy.spec?.podSelector?.matchExpressions, [{
    key: 'app.kubernetes.io/component',
    operator: 'In',
    values: ['api', 'worker']
  }]);

  const expected = new Map([
    ['sglang', 30000],
    ['mineru', 8001]
  ]);
  assert.equal(policy.spec?.egress?.length, expected.size);
  for (const rule of policy.spec.egress) {
    assert.equal(rule.to?.length, 1);
    const peer = rule.to[0];
    assert.equal(peer.ipBlock, undefined);
    assert.deepEqual(peer.namespaceSelector, {
      matchLabels: { 'kubernetes.io/metadata.name': 'ai-services' }
    });
    const provider = peer.podSelector?.matchLabels?.['app.kubernetes.io/name'];
    assert.ok(expected.has(provider), `unexpected provider selector: ${provider}`);
    assert.deepEqual(peer.podSelector, {
      matchLabels: { 'app.kubernetes.io/name': provider }
    });
    assert.deepEqual(rule.ports, [{ protocol: 'TCP', port: expected.get(provider) }]);
    expected.delete(provider);
  }
  assert.equal(expected.size, 0, 'every documented provider peer must be present once');
});

test('example overlay opts into base plus the custom AI component', async () => {
  const overlay = await loadYaml(
    'deploy/kubernetes/examples/custom-ai-egress/kustomization.yaml'
  );
  assert.equal(overlay.apiVersion, 'kustomize.config.k8s.io/v1beta1');
  assert.equal(overlay.kind, 'Kustomization');
  assert.deepEqual(overlay.resources, ['../../base']);
  assert.deepEqual(overlay.components, ['../../components/custom-ai-egress']);
});

const kubectlAvailable = spawnSync('kubectl', ['version', '--client'], {
  stdio: 'ignore'
}).status === 0;

test('kubectl renders one base policy and the second policy only after opt-in', {
  skip: !kubectlAvailable
}, () => {
  const render = (relativePath) => {
    const result = spawnSync(
      'kubectl',
      ['kustomize', fileURLToPath(new URL(relativePath, repositoryRoot))],
      { encoding: 'utf8' }
    );
    assert.equal(result.status, 0, result.stderr);
    return parseAllDocuments(result.stdout).map((document) => document.toJSON());
  };
  const policyNames = (documents) => documents
    .filter(({ kind }) => kind === 'NetworkPolicy')
    .map(({ metadata }) => metadata.name)
    .sort();
  assert.deepEqual(policyNames(render('deploy/kubernetes/base/')), [
    'paperless-archivist'
  ]);
  assert.deepEqual(policyNames(render('deploy/kubernetes/examples/custom-ai-egress/')), [
    'paperless-archivist',
    'paperless-archivist-custom-ai-egress'
  ]);
});
