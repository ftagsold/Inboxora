import { useTranslation } from 'react-i18next';
import { useStore } from '../store/index.ts';
import { retiredAddressOf } from '../utils/retiredAddresses.ts';

// A small muted tag naming the retired address a mail reached, e.g. "→ old@gmx.de".
export default function RetiredAddressTag({ to, cc, delivery, compact = false }: { to?: unknown; cc?: unknown; delivery?: unknown; compact?: boolean }) {
  const { t } = useTranslation();
  const retired = useStore(state => state.retiredAddresses);
  const match = retiredAddressOf(retired, to, cc, delivery);
  if (!match) return null;
  return (
    <span
      data-testid="retired-address-tag"
      title={t('message.retiredAddressHint', { address: match })}
      style={{
        display: 'inline-flex', alignItems: 'center', gap: 3, flexShrink: compact ? 1 : 0, minWidth: 0,
        maxWidth: compact ? 140 : undefined,
        fontSize: compact ? 10.5 : 11, lineHeight: 1.4, color: 'var(--text-tertiary)',
        border: '1px solid var(--border-subtle)', borderRadius: 4, padding: '0 5px',
        overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', verticalAlign: 'middle',
      }}
    >
      <span aria-hidden="true">↪</span>
      <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{match}</span>
    </span>
  );
}
