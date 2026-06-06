# Renovate — automated dependency updates

`renovate.json` (repo root) defines the policy: grouped patch/minor PRs per
ecosystem, **major updates as separate PRs** labelled `major-update`, lockfile
maintenance, and vulnerability alerts. It covers Cargo, npm/pnpm, the
`Dockerfile`, and `.gitlab-ci.yml`.

The config alone does nothing — a Renovate **bot** must run against this repo.
Pick one of:

## Option A — Mend Renovate GitLab app (simplest)
Enable the hosted Mend Renovate app for the `products/paperless-archivist`
group/project. It reads `renovate.json` and opens MRs automatically. No CI
changes or tokens stored in this repo.

## Option B — Self-hosted, scheduled CI pipeline (no external SaaS)
1. Create a project (or group) **access token** with `api` + `write_repository`
   scope. Store it as a **masked, protected CI/CD variable** named
   `RENOVATE_TOKEN`.
2. Add this job (a stage-less child or a dedicated pipeline). Gate it to
   scheduled runs so it never runs on normal MR/push pipelines:

   ```yaml
   renovate:
     stage: scan            # any existing stage
     image: renovate/renovate:41   # pin the major; Renovate updates itself via the bot
     rules:
       - if: '$CI_PIPELINE_SOURCE == "schedule"'
       - when: never
     variables:
       RENOVATE_PLATFORM: gitlab
       RENOVATE_ENDPOINT: "$CI_API_V4_URL"
       RENOVATE_TOKEN: "$RENOVATE_TOKEN"
       RENOVATE_AUTODISCOVER: "false"
       RENOVATE_REPOSITORIES: "$CI_PROJECT_PATH"
       LOG_LEVEL: info
     script:
       - renovate
   ```
3. Add a **Scheduled Pipeline** (CI/CD → Schedules), e.g. weekly `0 5 * * 1`.

Either way: patch/minor land as one grouped MR (CI-gated, safe to fast-track),
each major arrives as its own MR for deliberate review/testing.

## Known pending majors (let Renovate open these, review one at a time)
Rust 1.96 toolchain, `sqlx` 0.9, `reqwest` 0.13, `vite` 8, `typescript` 6,
`vitest` 4, `jsdom` 29, `@vitejs/plugin-react` 6, `lucide-react` 1.x,
`openapi-fetch` 0.17, Debian 12→13 (trixie) base image. See the majors
tracking issue.
