import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type InventoryItem, type InventoryQueryParams } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

const doc = (id: number, overrides: Partial<InventoryItem> = {}): InventoryItem => ({
  paperless_document_id: id,
  title: `Document ${id}`,
  current_tags: [],
  ocr_status: 'missing',
  metadata_status: 'missing',
  needs_review: false,
  complete: false,
  ...overrides
});

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      inventory: vi.fn(),
      triggerDocument: vi.fn(),
      bulkRerun: vi.fn(async (ids: number[]) => ({ queued: ids.length }))
    }
  };
});

const inventoryMock = vi.mocked(api.inventory);
type InventoryPage = Awaited<ReturnType<typeof api.inventory>>;

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

// Two pages (doc 1 on page one, doc 2 via "Load more"); single-id lookups
// return a refreshed row with a queued OCR status.
function paginatedInventory(params: InventoryQueryParams = {}): Promise<InventoryPage> {
  if (params.id != null) {
    return Promise.resolve({ items: [doc(params.id, { ocr_status: 'queued' })], total: 1, offset: 0, limit: 1 });
  }
  if (params.offset === 1) return Promise.resolve({ items: [doc(2)], total: 2, offset: 1, limit: 500 });
  return Promise.resolve({ items: [doc(1)], total: 2, offset: 0, limit: 500 });
}

async function renderInventory() {
  const { Inventory } = await import('./Inventory');
  const setError = vi.fn();
  const view = render(
    <I18nProvider>
      <Inventory setError={setError} />
    </I18nProvider>
  );
  return { setError, ...view };
}

describe('<Inventory> row actions and states', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.history.replaceState(null, '', '/inventory');
  });

  it('queues one run per double click, keeps loaded pages and patches the row (#426)', async () => {
    inventoryMock.mockImplementation(paginatedInventory);
    const trigger = deferred<{ run_id: string }>();
    vi.mocked(api.triggerDocument).mockReturnValue(trigger.promise);
    await renderInventory();
    await screen.findByRole('checkbox', { name: 'Select document #1' }, { timeout: 2_000 });
    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    await screen.findByRole('checkbox', { name: 'Select document #2' });
    inventoryMock.mockClear();

    const ocrButton = screen.getByRole('button', { name: 'Trigger OCR for document #1' });
    fireEvent.click(ocrButton);
    fireEvent.click(ocrButton);
    expect(api.triggerDocument).toHaveBeenCalledTimes(1);
    expect(ocrButton).toBeDisabled();
    expect(ocrButton).toHaveAttribute('aria-busy', 'true');

    await act(async () => trigger.resolve({ run_id: 'run-1' }));
    expect(await screen.findByText('Queued document #1 for processing.')).toBeInTheDocument();
    await waitFor(() => expect(ocrButton).toBeEnabled());

    // Only the affected row was re-fetched; both loaded pages are still shown.
    expect(inventoryMock).toHaveBeenCalledTimes(1);
    expect(inventoryMock).toHaveBeenCalledWith({ id: 1, offset: 0, limit: 1 });
    expect(screen.getByRole('checkbox', { name: 'Select document #2' })).toBeInTheDocument();
  });

  it('gives icon-only row actions accessible names (#426)', async () => {
    inventoryMock.mockImplementation(paginatedInventory);
    const { container } = await renderInventory();
    await screen.findByRole('checkbox', { name: 'Select document #1' }, { timeout: 2_000 });
    for (const name of [
      'Trigger OCR for document #1',
      'Trigger metadata for document #1',
      'Run full pipeline for document #1'
    ]) {
      expect(screen.getByRole('button', { name })).toBeInTheDocument();
    }
    const table = container.querySelector('table')!;
    expect(await axe(table)).toHaveNoViolations();
  });

  it('asks before a bulk re-run with the selected count (#417)', async () => {
    inventoryMock.mockImplementation(paginatedInventory);
    await renderInventory();
    fireEvent.click(await screen.findByRole('checkbox', { name: 'Select document #1' }, { timeout: 2_000 }));
    fireEvent.click(screen.getByRole('button', { name: 'Re-run selected' }));

    const dialog = await screen.findByRole('alertdialog', { name: 'Re-run 1 selected document(s)?' });
    expect(await axe(dialog)).toHaveNoViolations();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(api.bulkRerun).not.toHaveBeenCalled();
  });

  it('distinguishes loading, load failure and empty results (#429)', async () => {
    const first = deferred<InventoryPage>();
    inventoryMock.mockReturnValueOnce(first.promise);
    await renderInventory();

    const table = screen.getByRole('table', { name: 'Document Inventory' });
    expect(await within(table).findByRole('status')).toHaveTextContent('Loading documents');
    await waitFor(() => expect(inventoryMock).toHaveBeenCalledTimes(1), { timeout: 2_000 });
    await act(async () => first.reject(new Error('inventory unavailable')));

    const alert = await within(table).findByRole('alert');
    expect(alert).toHaveTextContent('Could not load documents');
    expect(within(table).queryByText('No documents match the current filters.')).not.toBeInTheDocument();

    inventoryMock.mockResolvedValueOnce({ items: [], total: 0, offset: 0, limit: 500 });
    fireEvent.click(within(alert).getByRole('button', { name: 'Retry' }));
    expect(await within(table).findByText('No documents match the current filters.')).toBeInTheDocument();
    expect(within(table).queryByRole('alert')).not.toBeInTheDocument();
  });
});
