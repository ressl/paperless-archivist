# Contributing

Paperless Archivist is a Rust and React project. Keep changes aligned with the
design documents in `docs/` and avoid direct writes to the Paperless database.

## Development Checks

Run the relevant checks before proposing changes:

```bash
cargo fmt --all
cargo test --workspace
cd frontend
pnpm install
pnpm typecheck
pnpm build
```

For security-sensitive work, also run secret scanning and dependency/container
checks used by CI when available.

## Public CI Policy

GitHub Actions is the canonical public CI gate for the official project. The
official GitLab.com repository is a source-distribution mirror and has project
CI/CD intentionally disabled, so an export does not duplicate the public build
or depend on a second runner quota.

The checked-in `.gitlab-ci.yml` remains supported as public-safe reference CI
for forks and downstream GitLab projects that enable it. Maintainers also run
the authoritative source pipeline and public-boundary/export checks before a
release. An official release therefore requires green GitHub Actions, a green
authoritative pipeline, and exact commit equality on both public mirrors; it
does not require a pipeline in the official GitLab.com mirror.

## Contribution Rules

- Use the Paperless REST API only.
- Keep frontend traffic behind the Archivist backend; do not call Paperless,
  Ollama, OpenAI, Anthropic, or Claude directly from the browser.
- Preserve audit logging for settings, security, review, and apply actions.
- Do not commit plaintext credentials, cluster access configs, age keys,
  database dumps, or decrypted SOPS files.
- `dev-samples/` and `scripts/.dev-local-*` contain real documents and live
  tokens pulled from the production stack. They are gitignored and excluded
  from the Docker build context via `.dockerignore`; never commit them
  (`git add -f` included) and never add a `COPY` that would ingest them.
- Add focused tests for critical domain logic, auth, validation, and worker
  idempotency.

## License

By contributing, you agree that your contribution is licensed under
`AGPL-3.0-or-later`.
