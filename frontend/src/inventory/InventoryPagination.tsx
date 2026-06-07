import { RefreshCw } from 'lucide-react';
import { useI18n } from '../i18n/I18nProvider';
import { ActionButton, run } from '../lib/ui';

type InventoryPaginationProps = {
  hasMore: boolean;
  busy: boolean;
  setBusy: (value: boolean) => void;
  setError: (error: string | null) => void;
  loadMore: () => Promise<unknown> | unknown;
};

export function InventoryPagination({ hasMore, busy, setBusy, setError, loadMore }: InventoryPaginationProps) {
  const { t } = useI18n();
  if (!hasMore) return null;
  return (
    <div className="toolbar">
      <ActionButton
        icon={<RefreshCw />}
        label={t('inventory.load_more')}
        busy={busy}
        onClick={() => run(setBusy, setError, loadMore, t)}
      />
    </div>
  );
}
