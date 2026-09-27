import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ApiError, api, type InventoryItem } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

const doc: InventoryItem = {
  paperless_document_id: 7,
  title: 'Doc seven',
  current_tags: [],
  ocr_status: 'succeeded',
  metadata_status: 'never',
  needs_review: false,
  complete: false
};

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      inventory: vi.fn(async () => ({ items: [doc], total: 1, offset: 0, limit: 500 })),
      inventoryMetadataTrace: vi.fn()
    }
  };
});

const traceMock = vi.mocked(api.inventoryMetadataTrace);

async function openDiagnose(setError: (value: string | null) => void) {
  const { Inventory } = await import('./Inventory');
  render(
    <I18nProvider>
      <Inventory setError={setError} />
    </I18nProvider>
  );
  fireEvent.click(await screen.findByTitle('Diagnose metadata', {}, { timeout: 2_000 }));
}

describe('<Inventory> diagnose error handling (#432)', () => {
  beforeEach(() => {
    cleanup();
    window.localStorage.clear();
    window.history.replaceState(null, '', '/inventory');
    traceMock.mockReset();
  });

  it('shows the "no run yet" state for a 404 ApiError, whatever its message', async () => {
    traceMock.mockRejectedValue(new ApiError('not found', 404));
    const setError = vi.fn();
    await openDiagnose(setError);
    expect(await screen.findByText('No metadata run has executed for this document yet.')).toBeInTheDocument();
    expect(setError).not.toHaveBeenCalledWith(expect.stringContaining('not found'));
  });

  it('reports other failures even if the text mentions "no metadata run"', async () => {
    traceMock.mockRejectedValue(new ApiError('no metadata run lock available', 500));
    const setError = vi.fn();
    await openDiagnose(setError);
    await vi.waitFor(() => expect(setError).toHaveBeenCalledWith('no metadata run lock available'));
    expect(screen.queryByText('No metadata run has executed for this document yet.')).not.toBeInTheDocument();
  });
});
