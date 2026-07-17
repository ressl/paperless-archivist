# Issue #355: Secure Caddy Profile Implementation Plan

**Goal:** Make the public Caddy/TLS Compose profile enforce secure browser cookies and HSTS without breaking the localhost HTTP profile.

### Task 1: Lock the deployment contract down with failing tests

**Files:**
- Create: `scripts/verify/compose_proxy_contract.mjs`
- Modify: `frontend/package.json`
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Add a rendered-Compose contract that distinguishes the local HTTP and proxy/TLS profiles.
- [ ] Add cookie unit coverage proving both the session and CSRF cookies gain `Secure` only in TLS mode.
- [ ] Run both tests before the implementation and record the expected proxy-profile failure.

### Task 2: Enforce the TLS profile

**Files:**
- Modify: `deploy/compose/docker-compose.proxy.yml`
- Verify: `deploy/compose/Caddyfile`

- [ ] Override `ARCHIVIST_COOKIE_SECURE=true` for the API in the proxy overlay.
- [ ] Verify the rendered local profile remains `false` and the rendered proxy profile is `true`.
- [ ] Validate the Caddy configuration and smoke-test HTTP redirect plus HTTPS HSTS/cookies.

### Task 3: Document operation and upgrade behavior

**Files:**
- Modify: `deploy/compose/README.md`
- Modify: `docs/INSTALLATION.md`
- Modify: `docs/RELEASE_NOTES.md`
- Modify: `docs/RELEASE_CHECKLIST.md`

- [ ] Document the local HTTP versus public HTTPS security boundary.
- [ ] Document HSTS and the secure-cookie behavior change, including restart and rollback implications.
- [ ] Add the proxy contract to the release checklist.

### Task 4: Verify and close

- [ ] Run focused contracts, API tests, formatting, Compose rendering, Caddy validation, and repository diff checks.
- [ ] Obtain independent review, commit, push, and close #355 only after the MR pipeline is green.
