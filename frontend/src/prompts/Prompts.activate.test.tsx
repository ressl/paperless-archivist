import { describe, expect, it, vi, beforeEach } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { I18nProvider } from '../i18n/I18nProvider';
import type { Prompt } from '../api/client';

// Two OCR prompt versions: v2 is active, v1 is the older draft the operator
// edits in the test. `activatePrompt` flips the active flag so the follow-up
// `load()` returns fresh array identities with changed content — exactly the
// reload that used to wipe unsaved editor state (#314).
let prompts: Prompt[];

function seedPrompts() {
  prompts = [
    {
      id: 'p1',
      stage: 'ocr',
      name: 'default',
      version: 1,
      content: 'original v1 content',
      active: false,
      created_at: '2026-06-01T00:00:00Z'
    },
    {
      id: 'p2',
      stage: 'ocr',
      name: 'default',
      version: 2,
      content: 'original v2 content',
      active: true,
      created_at: '2026-06-02T00:00:00Z'
    }
  ];
}

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      prompts: vi.fn(async () => ({ items: prompts.map((prompt) => ({ ...prompt })) })),
      promptUsage: vi.fn(async () => ({ items: [] })),
      promptExperiments: vi.fn(async () => ({ items: [] })),
      activatePrompt: vi.fn(async (id: string) => {
        prompts = prompts.map((prompt) => ({ ...prompt, active: prompt.id === id }));
        return {};
      })
    }
  };
});

describe('<Prompts> activate keeps unsaved edits', () => {
  beforeEach(() => {
    cleanup();
    seedPrompts();
  });

  it('does not discard a dirty editor when "Activate Selected" reloads (#314)', async () => {
    const { Prompts } = await import('./Prompts');
    render(
      <I18nProvider>
        <Prompts setError={() => undefined} />
      </I18nProvider>
    );

    // Wait for load(): the version dropdown (labelled "Version") starts on
    // the active prompt (v2). The compare dropdown renders earlier in the
    // loading state, so query by accessible name, not by order.
    const versionSelect = (await screen.findByRole('combobox', {
      name: 'Version'
    })) as HTMLSelectElement;
    await waitFor(() => expect(versionSelect.value).toBe('p2'));

    // Select the inactive v1 and edit its content -> editor is dirty.
    fireEvent.change(versionSelect, { target: { value: 'p1' } });
    const contentField = screen.getAllByRole('textbox').find(
      (el) => el.tagName === 'TEXTAREA' && (el as HTMLTextAreaElement).value === 'original v1 content'
    ) as HTMLTextAreaElement;
    expect(contentField).toBeDefined();
    fireEvent.change(contentField, { target: { value: 'edited draft content' } });
    expect(screen.getByText('unsaved edits')).toBeInTheDocument();

    // Activate the selected (still dirty) prompt; the background reload must
    // not reset the editor back to the stored content. Completion signal: the
    // reloaded items mark v1 active, which the version dropdown reflects in
    // its (localized) option label. The button also disables while the
    // request is in flight, so it cannot serve as the reload signal.
    fireEvent.click(screen.getByRole('button', { name: /Activate Selected/i }));
    await waitFor(() => {
      expect(screen.getByRole('option', { name: 'default v1 (active)' })).toBeInTheDocument();
    });
    expect(versionSelect.value).toBe('p1');
    expect(contentField.value).toBe('edited draft content');
    expect(screen.getByText('unsaved edits')).toBeInTheDocument();
  });
});
