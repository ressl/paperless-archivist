# Screenshot Workflow

Screenshots are part of the public project story. They must be repeatable,
public-safe, and updated when major UI areas change.

## Required Screens

- Dashboard with 24h range, live processing, charts, and maintenance panels.
- Settings with first-run wizard, Paperless test feedback, provider model
  dropdown, notifications, and workflow mode.
- Prompts with existing prompts visible and editor state.
- Inventory with processing statuses.
- Review queue with a synthetic suggestion.
- Document chat with synthetic citations.
- Audit view with non-sensitive events.

## Update Process

1. Prepare the synthetic demo environment from [Demo Plan](DEMO.md).
2. Use a clean browser profile and the English UI.
3. Hide or clear all secret inputs before capturing Settings.
4. Capture desktop and narrow viewport variants for major layout changes.
5. Store final images under `assets/screenshots/`.
6. Reference screenshots from `README.md` only after verifying the files contain
   no private data.

## Naming

Use stable names so README links do not churn:

```text
assets/screenshots/dashboard.png
assets/screenshots/settings.png
assets/screenshots/prompts.png
assets/screenshots/inventory.png
assets/screenshots/review.png
assets/screenshots/chat.png
assets/screenshots/audit.png
```

## Review Checklist

- No private domains, tokens, webhook URLs, OIDC secrets, or API keys.
- No real document titles or metadata.
- No browser extensions or personal bookmarks visible.
- UI language and feature labels match the current release.
- Images are readable at GitHub README width.
