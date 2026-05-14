# Branding

Status: draft  
Purpose: define the first visual direction for Paperless Archivist.

## 1. Brand Idea

Paperless Archivist should feel like a serious document intelligence product:

- reliable
- precise
- secure
- local-first
- enterprise-ready
- useful for private archives and professional document workflows

The brand should not feel playful, noisy, or like a generic AI toy.

## 2. Core Concept

The logo and visual language combine four ideas:

1. Archive: long-term document memory.
2. Document pages: the Paperless-ngx domain.
3. Structured intelligence: classification, OCR, fields, audit, and workflow.
4. AI core: models assist, but deterministic software controls the result.

Avoid overused AI clichés:

- robot faces
- generic brain icons
- glowing purple gradients
- magnifying glasses
- cluttered document stacks

## 3. Logo Asset

Current generated logo:

```text
assets/brand/paperless-archivist-logo.png
```

README usage:

```markdown
![Paperless Archivist logo](../assets/brand/paperless-archivist-logo.png)
```

The current logo is a generated bitmap concept. It is suitable for early project
identity, README, planning, and design exploration. Before a public release, the
logo should be converted into a clean vector source or redrawn as SVG.

## 4. Visual Style

Style direction:

- modern
- geometric
- crisp
- calm
- enterprise-ready
- high trust
- not decorative for its own sake

The mark should work as:

- GitLab/GitHub avatar
- favicon/app icon
- documentation header
- web app login screen symbol
- container/project icon

## 5. Color Direction

Recommended palette direction:

```text
Deep ink/navy       primary trust color
Warm off-white      background/base
Restrained teal     AI/status accent
Muted brass/gold    archive accent
Neutral slate       text and UI chrome
```

Avoid:

- dominant purple gradients
- neon AI colors
- beige-only palettes
- overly dark blue/slate-only UI
- loud rainbow accents

The product UI should remain operational and readable. The logo can carry a
small brass/teal accent, but the application itself should prioritize clarity.

## 6. Typography Direction

The wordmark should feel:

- technical but human
- stable
- readable at small sizes
- not futuristic in a gimmicky way

Recommended typography qualities:

- clean sans-serif
- moderate weight
- generous letter spacing only if still readable
- no script, serif ornament, or sci-fi display type

## 7. Image Generation Prompt

The first logo was generated from this design direction:

```text
Create a premium logo concept for an open-source product named "Paperless Archivist".
The product is an AI document intelligence companion for Paperless-ngx: OCR,
tagging, audit, review workflows, PostgreSQL, security, and local/API LLMs.

Logo requirements:
- Professional enterprise-ready brand mark, not playful.
- Combine the ideas of an archive, document pages, structured intelligence, and
  an AI core.
- Use a clean geometric symbol that can work as an app icon and GitHub project
  avatar.
- Avoid generic robot faces, brain cliches, magnifying glasses, and cluttered
  document stacks.
- Style: modern vector-like logo, crisp edges, balanced negative space, minimal
  but distinctive.
- Color direction: deep ink/navy, warm off-white, restrained teal or emerald
  accent, subtle brass/gold archive accent. Avoid purple gradients and overly
  bright neon.
- Include a standalone icon plus wordmark text "Paperless Archivist".
- Make the wordmark readable and spelled exactly: Paperless Archivist.
- Layout: centered on a clean light background, with enough whitespace.
- No mockup, no 3D, no shadows, no watermark.
```

## 8. Future Asset Requirements

Before first public release, create:

- source SVG logo
- horizontal logo
- icon-only logo
- light background variant
- dark background variant
- favicon set
- app icon PNG sizes
- social preview image
- README banner

Recommended files:

```text
assets/brand/logo.svg
assets/brand/logo-horizontal.svg
assets/brand/logo-icon.svg
assets/brand/logo-dark.svg
assets/brand/logo-light.svg
assets/brand/favicon.svg
assets/brand/social-preview.png
```

## 9. UI Branding Rules

The product UI should be functional first.

Rules:

- do not overuse logo graphics inside the app
- keep dashboard surfaces dense and readable
- use accent colors for status and action, not decoration
- avoid decorative gradient backgrounds
- avoid card-heavy marketing layouts in the application shell
- login screen may use the icon and concise product identity
- review and backlog screens should prioritize data clarity

## 10. Brand Voice

Tone:

- precise
- calm
- technical
- trustworthy
- not hype-driven

Good words:

- review
- audit
- classify
- extract
- process
- verify
- backlog
- confidence
- local

Avoid relying on:

- magic
- autonomous miracle claims
- vague AI productivity slogans
- playful mascot language
