# Public Mirror Governance Design

**Date:** 2026-07-17  
**Status:** Approved

## Goal

Make the supported public CI and mirror policy unambiguous while preserving a
reliable, least-privilege export path to both public forges.

## Supported policy

GitHub Actions is the canonical public CI gate for the official project. The
official GitLab.com repository is a source-distribution mirror and has project
CI/CD disabled, preventing duplicate or quota-dependent pipelines on every
export.

The repository keeps its public-safe `.gitlab-ci.yml`. This remains useful for
independent GitLab forks and for downstream users who intentionally enable
GitLab CI. Disabling CI is therefore an official-mirror project setting, not a
workflow rule embedded in the portable source tree.

Release documentation requires:

- green GitHub Actions for the exported commit;
- a green authoritative internal source pipeline;
- a successful public export and exact main-commit equality on both mirrors;
- no GitLab.com pipeline requirement for the official mirror.

## Export credential lifecycle

GitLab.com Free does not provide Project Access Tokens. The export therefore
uses a project-scoped service account whose personal access token has only the
`write_repository` scope. The service account is a member of this project only
and receives the lowest role that can push its protected default branch. The
project owner is the accountable human owner. The token has a fixed annual
expiration and is rotated at least one month before expiry.

The token value exists only in the internal source project's export variable.
That variable is protected, masked, hidden, and scoped to the public-export
environment. Its non-secret description records the owner, purpose, expiry,
and next rotation date. Token values are never placed in source, issues,
comments, job logs, or command output.

Rotation creates the replacement token first, updates the protected variable,
runs and verifies one export, and only then revokes the previous token. This
keeps rollback possible without extending exposure.

## Validation

An accepted change must prove all of the following:

1. CI/CD is disabled in the official GitLab.com mirror project;
2. the portable `.gitlab-ci.yml` remains syntactically valid for forks;
3. GitHub Actions succeeds for the expected exported commit;
4. the protected export job succeeds without leaking credential material;
5. GitHub and GitLab.com `main` resolve to the same expected commit;
6. public boundary scans remain clean;
7. the access token metadata and internal variable controls match this design.

If the official GitLab.com mirror is later promoted to a supported CI gate,
that is a policy change requiring restored runner capacity, a successful full
pipeline, updated release documentation, and a fresh governance review.
