# GA Release Checklist

Status: v1.0 launch checklist

Use this checklist before tagging a GA release.

## Version Bump

Keep the three version identities in lockstep with the release tag `vX.Y.Z`:

- [ ] `version` in `[workspace.package]` of the root `Cargo.toml` (all crates
      inherit it via `version.workspace = true`)
- [ ] `info.version` in `openapi/openapi.yaml`
- [ ] `version` in `frontend/package.json`

## Code And Tests

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `npm --prefix frontend run generate:client`
- [ ] `npm --prefix frontend run accessibility:check`
- [ ] `npm --prefix frontend run typecheck`
- [ ] `npm --prefix frontend run build`
- [ ] `node scripts/verify/docs_link_check.mjs`
- [ ] `docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml config`
- [ ] `docker compose --env-file deploy/compose/.env.example -f deploy/compose/docker-compose.yml -f deploy/compose/docker-compose.external-postgres.yml config`
- [ ] `kubectl kustomize deploy/kubernetes/base`

## Security And Public Safety

- [ ] Public boundary scan has no tree hits.
- [ ] Public boundary scan has no reachable commit-message hits.
- [ ] Public boundary scan has no reachable history hits.
- [ ] Public boundary scan has no tag hits.
- [ ] Public mirrors were verified from fresh clones.
- [ ] No real secrets or private deployment topology are present in docs.
- [ ] Provider, Paperless, and webhook errors redact URLs with credentials.

## Documentation

- [ ] README is current.
- [ ] User Guide is current.
- [ ] Operations Guide is current.
- [ ] Security policy/design is current.
- [ ] API Reference matches OpenAPI.
- [ ] AI Agent Guide explains boundaries and verification.
- [ ] Screenshots and demo plan are public-safe.
- [ ] Known limitations and roadmap are explicit.
- [ ] Release notes include upgrade and rollback notes.

## Release

- [ ] Public CI is green on GitHub.
- [ ] Public CI is green on GitLab.com.
- [ ] Internal source pipeline is green.
- [ ] Public export completed successfully.
- [ ] Release tag points at the verified commit.
- [ ] Release notes are attached to the release.
