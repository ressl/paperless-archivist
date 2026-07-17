import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { api, type InventoryItem, type InventoryQueryParams } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

const firstDocument: InventoryItem = {
  paperless_document_id: 1,
  title: 'First document',
  current_tags: [],
  ocr_status: 'succeeded',
  metadata_status: 'succeeded',
  needs_review: false,
  complete: true
};

const secondDocument: InventoryItem = {
  paperless_document_id: 2,
  title: 'Second document',
  current_tags: [],
  ocr_status: 'failed',
  metadata_status: 'queued',
  needs_review: true,
  complete: false
};

const stalePageDocument: InventoryItem = {
  paperless_document_id: 99,
  title: 'Stale page document',
  current_tags: [],
  ocr_status: 'succeeded',
  metadata_status: 'succeeded',
  needs_review: false,
  complete: true
};

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      inventory: vi.fn(async (params: InventoryQueryParams = {}) => {
        if (params.offset === 1) {
          return { items: [secondDocument], total: 2, offset: 1, limit: 500 };
        }
        if (params.q === 'Second document' || params.id === 2 || params.ocr_status?.includes('failed')) {
          return { items: [secondDocument], total: 1, offset: 0, limit: 500 };
        }
        return { items: [firstDocument], total: 2, offset: 0, limit: 500 };
      }),
      bulkRerun: vi.fn(async (documentIds: number[]) => ({ queued: documentIds.length }))
    }
  };
});

const inventoryMock = vi.mocked(api.inventory);
const bulkRerunMock = vi.mocked(api.bulkRerun);

async function renderInventory() {
  const { Inventory } = await import('./Inventory');
  render(
    <I18nProvider>
      <Inventory setError={() => undefined} />
    </I18nProvider>
  );
  return screen.findByRole('checkbox', { name: 'Select document #1' }, { timeout: 2_000 });
}

function selectedCount(count: number) {
  return screen.getByText(`${count} selected`);
}

describe('<Inventory> selection query context', () => {
  beforeEach(() => {
    cleanup();
    window.localStorage.clear();
    window.history.replaceState(null, '', '/inventory');
    inventoryMock.mockClear();
    bulkRerunMock.mockClear();
  });

  it('clears selected rows, count, and header state when a filter changes', async () => {
    const firstCheckbox = await renderInventory();
    const selectAll = screen.getByRole('checkbox', { name: 'Select all on this page' });
    fireEvent.click(firstCheckbox);
    expect(firstCheckbox).toBeChecked();
    expect(selectAll).toBeChecked();
    expect(selectedCount(1)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Failed OCR' }));

    expect(selectedCount(0)).toBeInTheDocument();
    expect(selectAll).not.toBeChecked();
    expect(screen.getByRole('button', { name: 'Re-run selected' })).toBeDisabled();
    expect(screen.queryByRole('checkbox', { name: 'Select document #1' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Load more' })).not.toBeInTheDocument();
    expect(await screen.findByRole('checkbox', { name: 'Select document #2' })).not.toBeChecked();
  });

  it('does not submit a hidden selection after committing a search', async () => {
    const firstCheckbox = await renderInventory();
    fireEvent.click(firstCheckbox);

    fireEvent.change(screen.getByRole('textbox', { name: 'Search by ID or title…' }), {
      target: { value: 'Second document' }
    });
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));

    const secondCheckbox = await screen.findByRole('checkbox', { name: 'Select document #2' });
    expect(selectedCount(0)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Re-run selected' })).toBeDisabled();
    expect(bulkRerunMock).not.toHaveBeenCalled();

    fireEvent.click(secondCheckbox);
    fireEvent.click(screen.getByRole('button', { name: 'Re-run selected' }));
    await waitFor(() => expect(bulkRerunMock).toHaveBeenCalledWith([2], ['ocr', 'metadata']));
  });

  it('removes the stale selection context as soon as refresh starts', async () => {
    const firstCheckbox = await renderInventory();
    fireEvent.click(firstCheckbox);
    expect(selectedCount(1)).toBeInTheDocument();

    let resolveRefresh!: (value: Awaited<ReturnType<typeof api.inventory>>) => void;
    inventoryMock.mockImplementationOnce(
      () => new Promise((resolve) => {
        resolveRefresh = resolve;
      })
    );

    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));

    expect(selectedCount(0)).toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: 'Select document #1' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Load more' })).not.toBeInTheDocument();
    resolveRefresh({ items: [firstDocument], total: 2, offset: 0, limit: 500 });

    expect(await screen.findByRole('checkbox', { name: 'Select document #1' })).not.toBeChecked();
    expect(selectedCount(0)).toBeInTheDocument();
  });

  it('invalidates an in-flight load-more response when the query changes', async () => {
    await renderInventory();
    let resolveOldPage!: (value: Awaited<ReturnType<typeof api.inventory>>) => void;
    inventoryMock.mockImplementationOnce(
      () => new Promise((resolve) => {
        resolveOldPage = resolve;
      })
    );

    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    fireEvent.click(screen.getByRole('button', { name: 'Failed OCR' }));
    expect(screen.queryByRole('checkbox', { name: 'Select document #1' })).not.toBeInTheDocument();

    resolveOldPage({ items: [stalePageDocument], total: 2, offset: 1, limit: 500 });
    await waitFor(() => expect(inventoryMock).toHaveBeenCalledTimes(3));

    expect(screen.queryByRole('checkbox', { name: 'Select document #99' })).not.toBeInTheDocument();
    expect(await screen.findByRole('checkbox', { name: 'Select document #2' })).not.toBeChecked();
  });

  it('preserves visible selections across load more without selecting appended rows', async () => {
    const firstCheckbox = await renderInventory();
    const selectAll = screen.getByRole('checkbox', { name: 'Select all on this page' });
    fireEvent.click(firstCheckbox);

    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));

    const secondCheckbox = await screen.findByRole('checkbox', { name: 'Select document #2' });
    expect(firstCheckbox).toBeChecked();
    expect(secondCheckbox).not.toBeChecked();
    expect(selectAll).not.toBeChecked();
    expect((selectAll as HTMLInputElement).indeterminate).toBe(true);
    expect(selectedCount(1)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Re-run selected' }));
    await waitFor(() => expect(bulkRerunMock).toHaveBeenCalledWith([1], ['ocr', 'metadata']));
  });
});
