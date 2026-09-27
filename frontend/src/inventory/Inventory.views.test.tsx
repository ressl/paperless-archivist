import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type InventoryItem, type InventorySavedView } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

expect.extend(toHaveNoViolations);

const doc: InventoryItem = {
  paperless_document_id: 9,
  title: 'Invoice nine',
  current_tags: [],
  ocr_status: 'succeeded',
  metadata_status: 'succeeded',
  needs_review: false,
  complete: true,
  correspondent_id: 7,
  correspondent_name: 'ACME Bank',
  document_type_id: 3,
  document_type_name: 'Invoice'
};

const savedView: InventorySavedView = {
  id: 'view-1',
  name: 'ACME invoices',
  query: 'correspondent=7&document_type=3',
  created_at: '2026-09-27T10:00:00Z',
  updated_at: '2026-09-27T10:00:00Z'
};

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      inventory: vi.fn(async () => ({ items: [doc], total: 1, offset: 0, limit: 500 })),
      paperlessCorrespondents: vi.fn(async () => ({ items: [{ id: 7, name: 'ACME Bank' }], truncated: false })),
      paperlessDocumentTypes: vi.fn(async () => ({ items: [{ id: 3, name: 'Invoice' }], truncated: false })),
      inventoryViews: vi.fn(async () => ({ items: [savedView] })),
      createInventoryView: vi.fn(async (name: string, query: string) => ({ ...savedView, id: 'view-2', name, query })),
      deleteInventoryView: vi.fn(async () => ({ ok: true }))
    }
  };
});

const inventoryMock = vi.mocked(api.inventory);

async function renderInventory() {
  const { Inventory } = await import('./Inventory');
  render(
    <I18nProvider>
      <Inventory setError={() => undefined} />
    </I18nProvider>
  );
  await screen.findByText('Invoice nine');
}

describe('<Inventory> correspondent/type filters, saved views and export (#447)', () => {
  beforeEach(() => {
    cleanup();
    vi.clearAllMocks();
    window.localStorage.clear();
    window.history.replaceState(null, '', '/inventory');
  });

  it('shows the Paperless names and filters by correspondent server-side', async () => {
    await renderInventory();
    expect(screen.getByText('ACME Bank · Invoice')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Advanced filters' }));
    const select = await screen.findByLabelText('Correspondent');
    await within(select).findByRole('option', { name: 'ACME Bank' });
    fireEvent.change(select, { target: { value: '7' } });
    await waitFor(
      () => expect(inventoryMock).toHaveBeenLastCalledWith(expect.objectContaining({ correspondent: ['7'] })),
      { timeout: 2_000 }
    );
    expect(window.location.search).toBe('?correspondent=7');
    const csv = screen.getByRole('link', { name: 'Export CSV' });
    expect(csv).toHaveAttribute('href', '/api/inventory/export?correspondent=7&format=csv');
    expect(screen.getByRole('link', { name: 'Export JSON' })).toHaveAttribute(
      'href',
      '/api/inventory/export?correspondent=7&format=json'
    );
  });

  it('applies a saved view and saves the current filters as a new one', async () => {
    await renderInventory();
    const picker = await screen.findByRole('combobox', { name: 'Saved views' });
    await within(picker).findByRole('option', { name: 'ACME invoices' });
    fireEvent.change(picker, { target: { value: 'view-1' } });
    await waitFor(
      () =>
        expect(inventoryMock).toHaveBeenLastCalledWith(
          expect.objectContaining({ correspondent: ['7'], document_type: ['3'] })
        ),
      { timeout: 2_000 }
    );

    fireEvent.click(screen.getByRole('button', { name: 'Save view' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'View name' }), { target: { value: 'Mine' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(api.createInventoryView).toHaveBeenCalledWith('Mine', '?correspondent=7&document_type=3')
    );
    expect(await screen.findByText('Saved view "Mine".')).toBeInTheDocument();
    expect(await axe(screen.getByRole('combobox', { name: 'Saved views' }).closest('.toolbar') as HTMLElement)).toHaveNoViolations();
  });

  it('confirms before deleting a saved view', async () => {
    await renderInventory();
    const picker = await screen.findByRole('combobox', { name: 'Saved views' });
    await within(picker).findByRole('option', { name: 'ACME invoices' });
    fireEvent.change(picker, { target: { value: 'view-1' } });
    fireEvent.click(await screen.findByRole('button', { name: 'Delete view' }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Delete saved view "ACME invoices"?' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(api.deleteInventoryView).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: 'Delete view' }));
    const again = await screen.findByRole('alertdialog');
    fireEvent.click(within(again).getByRole('button', { name: 'Delete view' }));
    await waitFor(() => expect(api.deleteInventoryView).toHaveBeenCalledWith('view-1'));
  });
});
