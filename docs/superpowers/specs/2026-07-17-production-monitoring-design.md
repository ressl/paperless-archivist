# Production Monitoring Design

**Date:** 2026-07-17  
**Status:** Approved

## Goal

Restore authenticated production scraping for Paperless Archivist and add
actionable alerts for queue pressure, failed jobs, and provider quota
exhaustion without exposing a metrics credential in source, rendered examples,
logs, or Git history.

## Boundary and ownership

The public source repository owns the application contract, a reusable opt-in
Prometheus Operator component, public-safe setup documentation, and offline
contract/rule tests. The private deployment repository owns the production
Secret reference, the concrete ServiceMonitor and PrometheusRule resources,
the production NetworkPolicy allowance, and the encrypted token value.

Only the API receives `ARCHIVIST_METRICS_TOKEN`; workers do not serve
`/metrics`. The existing endpoint contract remains unchanged:

- unset token: `503 Service Unavailable`;
- missing or invalid bearer token: `401 Unauthorized`;
- valid bearer token: `200 OK` with Prometheus text exposition.

## Public Kubernetes component

Add an opt-in `monitoring` Kustomize component and example. The component
contains:

- a ServiceMonitor selecting the API Service and reading the bearer credential
  from the existing application Secret;
- a PrometheusRule with alerts for sustained queue depth, terminal-job failure
  rate, scrape loss, and any provider-quota counter increase;
- no Secret object or credential value.

The generic base continues to allow ingress from the documented monitoring
namespace. Operators with different namespace or label conventions patch the
component in their private overlay.

Initial rules use deliberately conservative thresholds:

- queue depth greater than 100 for 30 minutes;
- more than five newly permanent job failures in 15 minutes;
- `increase(paperless_archivist_provider_quota_total[1h]) > 0` for 5 minutes.

The current failed-job metric is a mutable gauge and cannot safely be passed to
`increase()`. Add a monotone `paperless_archivist_job_failures_total` counter,
backed by `metrics_counters`, and increment it in the same transaction as each
permanent `job.failed` transition. Retries and lost-lease no-ops do not
increment it. This makes the recent failure-rate alert semantically correct.

## Production deployment

The production API Deployment references a dedicated SOPS-managed metrics
Secret. The ServiceMonitor reads the same key, so the application and
Prometheus cannot drift to different credentials and the worker receives no
unneeded credential. The production
CiliumNetworkPolicy admits only monitoring-stack pods to the API port in
addition to the existing ingress path. The private Kustomization includes the
ServiceMonitor and PrometheusRule.

The token is generated with a cryptographically secure random source and is
written only through the SOPS workflow. It must never be printed or passed on a
command line that can appear in process listings or CI logs.

## Validation

Public validation covers YAML parsing, Kustomize rendering, selector and Secret
reference contracts, alert expressions, and a `promtool test rules` case in
which a synthetic quota-counter increase reaches the firing state.

Production validation covers:

1. private Kustomize render and deploy pipeline;
2. Argo CD synchronization and API rollout health;
3. live `503`, `401`, and `200` endpoint behavior without revealing the token;
4. a healthy Prometheus target under the active NetworkPolicy;
5. loaded rule health and a reversible synthetic alert test;
6. log, render, and repository scans for credential leakage.

Rollback removes the monitoring resources and API environment reference while
leaving the encrypted Secret key harmlessly unused. Application health and the
existing endpoint-disabled `503` behavior remain available during rollback.
