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

Paperless API tokens and model-provider API keys are normally entered in the UI
and stored as encrypted secret references. Kubernetes operators may also seed
secret references through platform-specific automation, but the application must
still access Paperless and model providers only from the backend/worker.
