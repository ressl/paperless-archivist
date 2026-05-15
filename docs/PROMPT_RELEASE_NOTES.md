# Prompt Release Notes

Prompt versions are part of the product contract. Every default prompt change
should describe the stage, reason, expected quality impact, and regression
coverage before it is activated in production.

## 2026-05-15 - Quality Baseline

- Added golden-document fixtures for language and issue-date extraction.
- Added prompt regression tests for security wording, language context, strict
  JSON output, and deterministic temperature settings.
- Dashboard quality metrics now surface review acceptance, edits, rejections,
  uncertainty reviews, and provider/model feedback counts.

## Release Checklist

1. Add or update public-safe golden fixtures.
2. Run `cargo test`.
3. Test changed prompts in the Prompt Workbench with representative sample text.
4. Review Dashboard quality metrics after rollout.
5. Keep document content, customer names, private hostnames, and secrets out of
   fixtures and release notes.
