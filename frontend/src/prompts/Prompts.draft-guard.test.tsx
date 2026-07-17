import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { I18nProvider } from '../i18n/I18nProvider';
import type { Prompt } from '../api/client';

expect.extend(toHaveNoViolations);

let prompts: Prompt[];

function seedPrompts() {
  prompts = [
    {
      id: 'ocr-v1',
      stage: 'ocr',
      name: 'ocr-default',
      version: 1,
      content: 'OCR version one',
      active: false,
      created_at: '2026-06-01T00:00:00Z'
    },
    {
      id: 'ocr-v2',
      stage: 'ocr',
      name: 'ocr-default',
      version: 2,
      content: 'OCR version two',
      active: true,
      created_at: '2026-06-02T00:00:00Z'
    },
    {
      id: 'metadata-v1',
      stage: 'metadata',
      name: 'metadata-default',
      version: 1,
      content: 'Metadata version one',
      active: true,
      created_at: '2026-06-03T00:00:00Z'
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
      promptExperiments: vi.fn(async () => ({ items: [] }))
    }
  };
});

async function renderWorkbench() {
  const { Prompts } = await import('./Prompts');
  render(
    <I18nProvider>
      <Prompts setError={() => undefined} />
    </I18nProvider>
  );
  const version = (await screen.findByRole('combobox', { name: 'Version' })) as HTMLSelectElement;
  const content = screen.getByRole('textbox', { name: 'Prompt content' }) as HTMLTextAreaElement;
  // The selected prompt is derived as soon as the prompt list loads, while a
  // following effect synchronises the editor fields from that selection. Wait
  // for both pieces of initial state before a test starts interacting; waiting
  // for the select alone can click during that one-render hand-off and exercise
  // the dirty-draft guard instead of the intended clean navigation path.
  await waitFor(() => {
    expect(version.value).toBe('ocr-v2');
    expect(content.value).toBe('OCR version two');
  });
  return { version, content };
}

function metadataStageButton() {
  return within(screen.getByRole('complementary', { name: 'Prompt stages' })).getByRole('button', {
    name: /^Metadata/
  });
}

describe('<Prompts> unsaved draft navigation guard', () => {
  beforeEach(() => {
    cleanup();
    seedPrompts();
  });

  it('changes clean versions and stages immediately without a dialog', async () => {
    const { version, content } = await renderWorkbench();

    fireEvent.click(screen.getByRole('button', { name: /^ocr-default v1/ }));
    await waitFor(() => expect(content.value).toBe('OCR version one'));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();

    fireEvent.click(metadataStageButton());
    await waitFor(() => expect(version.value).toBe('metadata-v1'));
    expect(content.value).toBe('Metadata version one');
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('cancels a dirty stage switch and preserves the selection and draft', async () => {
    const { version, content } = await renderWorkbench();
    fireEvent.change(content, { target: { value: 'operator stage draft' } });

    fireEvent.click(metadataStageButton());
    const dialog = await screen.findByRole('alertdialog', { name: 'Discard unsaved changes?' });
    expect(dialog).toHaveAccessibleDescription(
      'Switching the prompt stage or version will replace your current draft.'
    );
    expect(await axe(dialog)).toHaveNoViolations();

    fireEvent.click(within(dialog).getByRole('button', { name: 'Keep editing' }));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
    expect(version.value).toBe('ocr-v2');
    expect(content.value).toBe('operator stage draft');
  });

  it('discards a dirty stage draft and switches to the pending stage once', async () => {
    const { version, content } = await renderWorkbench();
    fireEvent.change(content, { target: { value: 'discard this stage draft' } });

    fireEvent.click(metadataStageButton());
    fireEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', {
        name: 'Discard and switch'
      })
    );

    await waitFor(() => expect(version.value).toBe('metadata-v1'));
    expect(content.value).toBe('Metadata version one');
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('guards an unsaved activate-after-save choice even when text is unchanged', async () => {
    const { version } = await renderWorkbench();
    const activate = screen.getByRole('checkbox', {
      name: 'Activate after save'
    }) as HTMLInputElement;
    expect(activate.checked).toBe(true);
    fireEvent.click(activate);

    fireEvent.click(metadataStageButton());
    const dialog = await screen.findByRole('alertdialog');
    expect(version.value).toBe('ocr-v2');
    expect(activate.checked).toBe(false);

    fireEvent.click(within(dialog).getByRole('button', { name: 'Keep editing' }));
    expect(version.value).toBe('ocr-v2');
    expect(activate.checked).toBe(false);
  });

  it('resets a checkbox-only draft to the synced activate baseline', async () => {
    await renderWorkbench();
    const activate = screen.getByRole('checkbox', {
      name: 'Activate after save'
    }) as HTMLInputElement;
    fireEvent.click(activate);
    expect(screen.getByText('unsaved edits')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Reset' }));

    expect(activate.checked).toBe(true);
    expect(screen.queryByText('unsaved edits')).not.toBeInTheDocument();
  });

  it('cancels a dirty version switch with Escape and restores focus', async () => {
    const { version, content } = await renderWorkbench();
    fireEvent.change(content, { target: { value: 'operator version draft' } });
    version.focus();

    fireEvent.change(version, { target: { value: 'ocr-v1' } });
    const dialog = await screen.findByRole('alertdialog', { name: 'Discard unsaved changes?' });
    await waitFor(() =>
      expect(within(dialog).getByRole('button', { name: 'Keep editing' })).toHaveFocus()
    );
    fireEvent.keyDown(window, { key: 'Escape' });

    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(version).toHaveFocus();
    expect(version.value).toBe('ocr-v2');
    expect(content.value).toBe('operator version draft');
  });

  it('discards a dirty version draft and applies the pending version once', async () => {
    const { version, content } = await renderWorkbench();
    fireEvent.change(content, { target: { value: 'discard this version draft' } });

    fireEvent.click(screen.getByRole('button', { name: /^ocr-default v1/ }));
    fireEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', {
        name: 'Discard and switch'
      })
    );

    await waitFor(() => expect(version.value).toBe('ocr-v1'));
    expect(content.value).toBe('OCR version one');
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });
});
