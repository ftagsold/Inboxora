// Scheduled mail at the top of an account's Sent folder. Each row carries a small clock and
// the time it goes out; clicking one opens it in the reader (ScheduledReader), from where it
// can be rescheduled, edited or deleted. The list refetches when the server reports a change
// (`scheduled_changed` over WebSocket).

import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import { useStore } from '../store/index.ts';
import { senderColor } from '../themes.ts';
import { formatScheduled } from '../utils/scheduleSend.ts';

export interface ScheduledRow {
  id: string;
  account_id: string;
  send_at: string;
  status: 'scheduled' | 'sending' | 'failed';
  last_error?: string | null;
  to?: string[] | string | null;
  cc?: string[] | string | null;
  bcc?: string[] | string | null;
  subject?: string | null;
  attachments?: string[];
}

export function recipientList(v: ScheduledRow['to']): string[] {
  if (Array.isArray(v)) return v.filter(Boolean).map(String);
  return typeof v === 'string' && v.trim() ? [v] : [];
}

/** "Name <a@b>" → "Name"; a bare address stays as is. */
export function displayName(address: string): string {
  const m = /^\s*"?([^"<]+?)"?\s*<[^>]+>\s*$/.exec(address);
  return (m ? m[1] : address).trim();
}

export function ClockIcon({ size = 11 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="9" /><polyline points="12 7 12 12 15 14" />
    </svg>
  );
}

export default function ScheduledSection({ accountId }: { accountId: string }) {
  const { t, i18n } = useTranslation();
  const locale = i18n.resolvedLanguage || i18n.language;
  const [rows, setRows] = useState<ScheduledRow[]>([]);
  const selectedId = useStore(s => s.selectedScheduledId);
  const setSelectedScheduled = useStore(s => s.setSelectedScheduled);

  const load = useCallback(() => {
    api.getScheduled(accountId)
      .then((data: { scheduled?: ScheduledRow[] }) => setRows(Array.isArray(data?.scheduled) ? data.scheduled : []))
      .catch(() => {});
  }, [accountId]);

  useEffect(() => {
    load();
    const onChange = (e: Event) => {
      const changed = (e as CustomEvent<{ accountId?: string }>).detail?.accountId;
      if (!changed || changed === accountId) load();
    };
    window.addEventListener('inboxora:scheduled_changed', onChange);
    return () => window.removeEventListener('inboxora:scheduled_changed', onChange);
  }, [accountId, load]);

  if (!rows.length) return null;

  return (
    <div data-testid="scheduled-section" style={{ borderBottom: '1px solid var(--border)' }}>
      <div style={{
        padding: '8px var(--layout-row-px, 14px) 4px', fontSize: 11, fontWeight: 600, letterSpacing: '0.05em',
        color: 'var(--text-tertiary)', textTransform: 'uppercase',
      }}>
        {t('scheduled.heading', { n: rows.length })}
      </div>
      {rows.map(row => {
        const to = recipientList(row.to);
        const first = to[0] ? displayName(to[0]) : t('scheduled.noRecipient');
        const failed = row.status === 'failed';
        const color = senderColor(to[0] || first);
        const selected = row.id === selectedId;
        return (
          <div
            key={row.id}
            role="button"
            tabIndex={0}
            data-testid="scheduled-row"
            aria-current={selected || undefined}
            onClick={() => setSelectedScheduled(row.id)}
            onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); setSelectedScheduled(row.id); } }}
            className={`scheduled-row${selected ? ' is-selected' : ''}`}
            style={{ position: 'relative', padding: 'var(--layout-row-py, 11px) var(--layout-row-px, 14px)', cursor: 'pointer', display: 'flex', gap: 10, alignItems: 'flex-start' }}
          >
            {selected && <div style={{ position: 'absolute', left: 0, top: 0, bottom: 0, width: 3, background: 'var(--accent)', borderRadius: '0 2px 2px 0' }} />}
            <div style={{
              width: 36, height: 36, borderRadius: '50%', flexShrink: 0, boxSizing: 'border-box', marginTop: 1,
              background: `${color}22`, border: `1.5px solid ${color}55`, color,
              display: 'flex', alignItems: 'center', justifyContent: 'center', fontSize: 14, fontWeight: 600,
            }}>
              {first[0]?.toUpperCase() || '?'}
            </div>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8, marginBottom: 3 }}>
                <span style={{ fontSize: 13, fontWeight: 500, color: 'var(--text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {to.length > 1 ? `${first} +${to.length - 1}` : first}
                </span>
                <span
                  title={failed ? t('scheduled.failed') : t('scheduled.sendsAt', { when: formatScheduled(row.send_at, locale) })}
                  style={{
                    display: 'inline-flex', alignItems: 'center', gap: 4, flexShrink: 0,
                    fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10.5,
                    color: failed ? 'var(--red)' : 'var(--accent)',
                  }}
                >
                  <ClockIcon />
                  {formatScheduled(row.send_at, locale)}
                </span>
              </div>
              <div style={{ fontSize: 13, color: 'var(--text-max-contrast)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', marginBottom: 3 }}>
                {row.subject || t('message.noSubject')}
              </div>
              <div style={{ fontSize: 12, color: failed ? 'var(--red)' : 'var(--text-tertiary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {failed ? (row.last_error || t('scheduled.failed')) : row.status === 'sending' ? t('scheduled.sending') : t('scheduled.notYetSent')}
              </div>
            </div>
          </div>
        );
      })}
    </div>
  );
}
