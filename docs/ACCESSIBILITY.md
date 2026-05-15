# Accessibility Audit

Status: v1.0 GA audit

Paperless Archivist is an operational tool. The v1.0 accessibility target is
usable keyboard navigation, visible focus, labelled inputs, non-crashing
tooltips, screen-reader-friendly feedback, and sufficient color contrast for the
core workflows.

## Audited Areas

| Area | v1.0 status |
| --- | --- |
| Login | Labelled username/password fields, language selector, SSO link, visible focus. |
| Dashboard | Main landmark, labelled range tabs, labelled workflow mode controls, live status text. |
| Inventory | Reload and trigger actions use accessible button names and table text. |
| Review | Selection checkbox, batch actions, editable fields, approve/reject actions. |
| Settings | Labelled inputs, provider model dropdown labels, connection feedback live region. |
| Prompts | Keyboard-focusable stage rail, prompt editor labels, tooltip escape/outside close. |
| Chat | Labelled document IDs and question fields, session list buttons. |
| Users/Tokens | Labelled create/reset/rotate/revoke controls. |

## Implemented Fixes

- Global `:focus-visible` styles for buttons, links, inputs, selects, and
  textareas.
- Provider model dropdowns have explicit accessible labels.
- Ollama refresh icon button has an accessible label.
- Admin user and API token forms no longer rely only on placeholders.
- Tooltip components support hover, focus, tap, Escape, and outside click/tap.
- Connection-test feedback uses `aria-live="polite"` and status/alert roles.

## Automated Check

The project includes a no-dependency static guard for critical accessibility
markers:

```bash
npm --prefix frontend run accessibility:check
```

The check is intentionally small and fast. It does not replace manual testing or
browser-based audits, but it prevents accidental removal of core accessibility
patterns.

## Manual Keyboard Checklist

Before a GA release:

1. Log in using keyboard only.
2. Move through sidebar navigation with Tab and Shift+Tab.
3. Change dashboard range and workflow mode using keyboard only.
4. Open Settings, test Paperless/provider, and confirm feedback is announced
   visually and remains readable.
5. Open both model recommendation and prompt info tooltips, then close with Esc.
6. Select review items, edit a field, and approve/reject using keyboard only.
7. Create and revoke an API token using keyboard only.

## Known Limitations

- Full screen-reader testing should be repeated with the exact browser/OS
  combinations used by operators.
- Data-heavy tables are horizontally scrollable on small screens; mobile
  operation is supported for administration, but desktop remains the primary
  layout for bulk review.
- The static guard cannot calculate color contrast dynamically. Color changes
  should be checked manually before release.
