# Generic Kubernetes Package

This package is public-safe and intentionally generic. It is for users who want
Kubernetes manifests without any private deployment topology.

## What It Contains

- API/UI Deployment with `/healthz` and `/readyz` probes
- Worker Deployment with separate resources and command
- Service, Ingress, ServiceAccount, and NetworkPolicy
- ConfigMap for non-secret runtime settings
- example Secret manifest for required secret keys
- `values.example.yaml` documenting the settings that operators usually patch
- opt-in Prometheus Operator monitoring and custom-AI egress components

The package does not include PostgreSQL, Paperless-ngx, Ollama, certificate
issuers, DNS, or private registry automation. Bring those from your platform.

## Render And Apply

Review and patch the examples first:

```bash
cp deploy/kubernetes/secret.example.yaml /tmp/paperless-archivist-secret.yaml
$EDITOR /tmp/paperless-archivist-secret.yaml
kubectl apply -n paperless-archivist -f /tmp/paperless-archivist-secret.yaml
kubectl apply -n paperless-archivist -k deploy/kubernetes/base
```

For production, use your secret manager instead of committing the Secret
manifest. Patch image repository, image tag, ingress host, OIDC settings, and
resource limits through Kustomize overlays or your GitOps system.

## Opt-in Egress To Custom AI Providers

The base NetworkPolicy deliberately does not open arbitrary in-cluster model
ports. If an enforcing CNI protects the namespace, opt into
[`components/custom-ai-egress`](components/custom-ai-egress/) only after
adapting it in your own overlay. The component adds a second policy; Kubernetes
unions its egress with the base policy instead of replacing or weakening the
base.

The checked-in values are public examples, not production defaults:

| Example | Namespace label | Provider pod label | Example TCP target port |
| --- | --- | --- | --- |
| SGLang | `kubernetes.io/metadata.name=ai-services` | `app.kubernetes.io/name=sglang` | `30000` |
| MinerU | `kubernetes.io/metadata.name=ai-services` | `app.kubernetes.io/name=mineru` | `8001` |

The policy selects both Archivist backend components through
`app.kubernetes.io/component In (api,worker)`: the API needs provider tests and
document chat, and the worker needs model-backed processing. It grants neither
frontend nor arbitrary namespace egress. Each destination is an intersection
of a namespace selector, a pod selector, and one TCP port; there is no empty
namespace selector, private CIDR, or `ipBlock` escape hatch.

Before using it, copy or patch the component in your private GitOps overlay and
replace all three target facts with values observed in your cluster. Use the
provider Service `targetPort`, not an assumed port copied from this example:

```bash
kubectl get namespace ai-services --show-labels
kubectl -n ai-services get pods --show-labels
kubectl -n ai-services get service sglang mineru \
  -o custom-columns=NAME:.metadata.name,PORT:.spec.ports[*].port,TARGET:.spec.ports[*].targetPort
```

Render the unchanged base and the opt-in example separately before applying
your adapted overlay:

```bash
kubectl kustomize deploy/kubernetes/base > /tmp/archivist-base.yaml
kubectl kustomize deploy/kubernetes/examples/custom-ai-egress \
  > /tmp/archivist-custom-ai.yaml
diff -u /tmp/archivist-base.yaml /tmp/archivist-custom-ai.yaml
kubectl apply -n paperless-archivist -k path/to/your/adapted-overlay
```

The diff must add only `paperless-archivist-custom-ai-egress`; the base policy
must still contain only its original DNS, PostgreSQL, Paperless, Ollama, and
external-HTTPS rules. Confirm the effective selectors and then exercise both
callers: run the provider test or document chat through the API, and run one
worker job. No Paperless document or real provider credential is needed for
the render checks.

If DNS works but provider requests time out, treat it as a likely policy or
Service-target mismatch. Check, in order:

```bash
kubectl -n paperless-archivist describe networkpolicy \
  paperless-archivist-custom-ai-egress
kubectl -n ai-services get endpointslice -l kubernetes.io/service-name=sglang -o wide
kubectl -n ai-services get endpointslice -l kubernetes.io/service-name=mineru -o wide
kubectl -n paperless-archivist get pods \
  -l app.kubernetes.io/name=paperless-archivist --show-labels
```

Verify the actual namespace label, destination pod labels, endpoint ports, and
protocol against the policy. An empty EndpointSlice is a Service problem, not
a NetworkPolicy problem. When those match, inspect your CNI flow logs for a
drop from the API and worker pod identities to the selected provider pod and
port. Cilium/Hubble, Calico flow logs, and other enforcing CNIs expose this in
different tools; follow the CNI-specific procedure rather than adding a broad
namespace or RFC1918 allow rule.

## Opt-in Authenticated Monitoring

[`components/monitoring`](components/monitoring/) adds a ServiceMonitor, a
PrometheusRule, and an API Deployment patch for `ARCHIVIST_METRICS_TOKEN`. It is
opt-in because clusters without Prometheus Operator CRDs cannot accept those
resource kinds. The unchanged base remains portable.

The component expects a dedicated Secret named
`paperless-archivist-metrics`, with key `ARCHIVIST_METRICS_TOKEN`, in the same
namespace as the application and ServiceMonitor. The API and Prometheus read
the same key; the worker does not receive it. Generate the real value with a
cryptographically secure secret manager and never commit it. The second
document in `secret.example.yaml` shows the required shape only.

Render the opt-in example before adapting it:

```bash
kubectl kustomize deploy/kubernetes/base > /tmp/archivist-base.yaml
kubectl kustomize deploy/kubernetes/examples/monitoring \
  > /tmp/archivist-monitoring.yaml
```

Patch the ServiceMonitor labels if the Prometheus installation applies a
selector, and patch the NetworkPolicy monitoring namespace if it differs from
the base example. Confirm the effective Prometheus pod labels and admit only
that identity to TCP/8080 in a private overlay; do not open the API port to all
namespaces.

The initial rules alert on scrape loss, a queue above 100 for 30 minutes, more
than five new permanent failures in 15 minutes, and any provider-quota event in
one hour. Validate custom thresholds with the local workload before changing
them. The checked-in synthetic rule fixture proves that a rising quota counter
fires and a flat counter stays quiet:

```bash
docker run --rm -v "$PWD/scripts/verify:/workspace" -w /workspace \
  --entrypoint /bin/promtool \
  quay.io/prometheus/prometheus@sha256:69f5241418838263316593f7274a304b095c40bcf22e57272865da91bd60a8ac \
  test rules paperless_archivist_alert_rules.test.yaml
```

Rotate the metrics token by updating the dedicated Secret and restarting the
API; the ServiceMonitor observes the same Secret key. Verify authenticated
`200` and target health before removing the old secret version. Rollback removes
the component and environment patch; `/metrics` then safely returns `503`.

## Migration Order

1. Upgrade PostgreSQL compatibility first; Paperless Archivist requires
   PostgreSQL 18.
2. Apply the secret/config changes.
3. Deploy the API and wait for `/readyz`. The API runs migrations.
4. Deploy or scale the worker after the API is ready.
5. Run Paperless sync and one test job before enabling autopilot.

Rollback rule: roll back the application image before rolling back database
state. Do not downgrade PostgreSQL. If a migration was applied, restore from a
database backup only after stopping API and worker pods.

## Secret References

Required secret keys:

- `DATABASE_URL`
- `ARCHIVIST_SECRET_KEY`
- either `ARCHIVIST_ADMIN_PASSWORD` for bootstrap login or OIDC settings for SSO
- optional `ARCHIVIST_OIDC_CLIENT_SECRET`
- optional dedicated `paperless-archivist-metrics` Secret with
  `ARCHIVIST_METRICS_TOKEN` when the monitoring component is enabled

Paperless API tokens and model-provider API keys are normally entered in the UI
and stored as encrypted secret references. Kubernetes operators may also seed
secret references through platform-specific automation, but the application must
still access Paperless and model providers only from the backend/worker.
