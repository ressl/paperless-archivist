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

async function loadYaml(relativePath) {
  return parse(await readFile(new URL(relativePath, repositoryRoot), 'utf8'));
}

test('monitoring component uses one dedicated Secret for API and Prometheus', async () => {
  const patch = await loadYaml(
    'deploy/kubernetes/components/monitoring/deployment-api-metrics-env-patch.yaml'
  );
  assert.deepEqual(patch, [{
    op: 'add',
    path: '/spec/template/spec/containers/0/env',
    value: [{
      name: 'ARCHIVIST_METRICS_TOKEN',
      valueFrom: {
        secretKeyRef: {
          name: 'paperless-archivist-metrics',
          key: 'ARCHIVIST_METRICS_TOKEN'
        }
      }
    }]
  }]);

  const component = await loadYaml(
    'deploy/kubernetes/components/monitoring/kustomization.yaml'
  );
  assert.deepEqual(component.patches[0].target, {
    group: 'apps',
    version: 'v1',
    kind: 'Deployment',
    name: 'paperless-archivist-api'
  });

  const monitor = await loadYaml(
    'deploy/kubernetes/components/monitoring/servicemonitor.yaml'
  );
  assert.equal(monitor.kind, 'ServiceMonitor');
  assert.deepEqual(monitor.spec.selector.matchLabels, {
    'app.kubernetes.io/name': 'paperless-archivist',
    'app.kubernetes.io/component': 'api'
  });
  assert.deepEqual(monitor.spec.endpoints[0].authorization, {
    type: 'Bearer',
    credentials: {
      name: 'paperless-archivist-metrics',
      key: 'ARCHIVIST_METRICS_TOKEN'
    }
  });
  assert.deepEqual(monitor.spec.endpoints[0].relabelings, [{
    action: 'replace',
    targetLabel: 'paperless_archivist_instance',
    replacement: 'paperless-archivist'
  }]);
});

test('supported alert rules include scrape, queue, permanent failures and quota', async () => {
  const rule = await loadYaml(
    'deploy/kubernetes/components/monitoring/prometheusrule.yaml'
  );
  const raw = await loadYaml('scripts/verify/paperless_archivist_alert_rules.yaml');
  assert.equal(rule.kind, 'PrometheusRule');
  assert.deepEqual(rule.spec.groups, raw.groups, 'promtool fixture must match the CR');

  const alerts = new Map(
    rule.spec.groups.flatMap((group) => group.rules).map((entry) => [entry.alert, entry])
  );
  assert.equal(alerts.size, 4);
  assert.match(alerts.get('PaperlessArchivistScrapeDown').expr, /absent\(up/);
  assert.match(alerts.get('PaperlessArchivistQueueBacklog').expr, /jobs_queued/);
  assert.match(alerts.get('PaperlessArchivistJobFailureRateHigh').expr, /job_failures_total/);
  assert.match(alerts.get('PaperlessArchivistProviderQuotaExhausted').expr, /provider_quota_total/);
  for (const rule of alerts.values()) {
    assert.match(
      rule.expr,
      /paperless_archivist_instance="paperless-archivist"/,
      `${rule.alert} must be scoped to the stable target label`
    );
    assert.doesNotMatch(rule.expr, /service="paperless-archivist"/);
  }
  assert.doesNotMatch(
    alerts.get('PaperlessArchivistJobFailureRateHigh').expr,
    /jobs_failed/,
    'the mutable failed-jobs gauge must not be used with increase()'
  );
});

const kubectlAvailable = spawnSync('kubectl', ['version', '--client'], {
  stdio: 'ignore'
}).status === 0;

test('monitoring remains opt-in and renders only with its CRDs', {
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
  const baseKinds = render('deploy/kubernetes/base/').map(({ kind }) => kind);
  assert.ok(!baseKinds.includes('ServiceMonitor'));
  assert.ok(!baseKinds.includes('PrometheusRule'));

  const monitored = render('deploy/kubernetes/examples/monitoring/');
  assert.equal(monitored.filter(({ kind }) => kind === 'ServiceMonitor').length, 1);
  assert.equal(monitored.filter(({ kind }) => kind === 'PrometheusRule').length, 1);
  const api = monitored.find(({ kind, metadata }) =>
    kind === 'Deployment' && metadata.name === 'example-paperless-archivist-api'
  );
  assert.ok(api, 'namePrefix render must retain the API deployment and monitoring patch');
  assert.ok(api.spec.template.spec.containers[0].env.some(
    ({ name }) => name === 'ARCHIVIST_METRICS_TOKEN'
  ));

  const customInstance = render(
    'deploy/kubernetes/examples/monitoring-custom-instance/'
  );
  const customMonitor = customInstance.find(({ kind }) => kind === 'ServiceMonitor');
  assert.equal(
    customMonitor.spec.endpoints[0].relabelings[0].replacement,
    'another-archivist'
  );
  const customRule = customInstance.find(({ kind }) => kind === 'PrometheusRule');
  const customAlerts = customRule.spec.groups.flatMap(({ rules }) => rules);
  assert.equal(customAlerts.length, 4);
  for (const rule of customAlerts) {
    assert.match(
      rule.expr,
      /paperless_archivist_instance="another-archivist"/,
      `${rule.alert} must select the customized target label`
    );
    assert.doesNotMatch(
      rule.expr,
      /paperless_archivist_instance="paperless-archivist"/,
      `${rule.alert} must not retain the component default`
    );
  }
});
