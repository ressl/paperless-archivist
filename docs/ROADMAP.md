# Roadmap And Known Limitations

Status: public roadmap

## v1.0.x

- Documentation polish and public onboarding improvements.
- Additional translation catalogs.
- More browser-based accessibility checks.
- Better visual screenshot set and demo media.
- Additional provider model catalog refreshes.

## Later Candidates

- Optional model benchmark helper for local Ollama.
- More notification providers beyond generic webhook.
- Richer Paperless multi-archive profile management.
- Public demo environment with synthetic documents.
- More advanced document-chat retrieval strategies.

## Known Limitations

- Archivist depends on Paperless API behavior and permissions. Paperless outages
  or token issues stop sync and apply operations.
- Local vision models can fail because of upstream model/runtime issues. Use
  the provider test and recent failure panel to identify model crashes.
- Full autopilot should only be enabled after validation and review results
  match the archive's metadata policy.
- Table-heavy workflows are optimized for desktop operators.
