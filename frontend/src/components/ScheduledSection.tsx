// Scheduled mail at the top of an account's Sent folder. Each row carries a small clock and
// the time it goes out; opening one shows a preview from which the send can be cancelled.
// The list refetches when the server reports a change (`scheduled_changed` over WebSocket).

import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import DOMPurify from 'dompurify';
import { api } from '../utils/api.ts';
import { senderColor } from '../themes.ts';
import { formatScheduled } from '../utils/scheduleSend.ts';
import { toAppError } from '../utils/errors.ts';
import { Button, Dialog } from './ui.tsx';

interface ScheduledRow {
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

interface ScheduledDetail extends ScheduledRow { body?: string | null; body_is_html?: boolean }

function list(v: ScheduledRow['to']): string[] {
  if (Array.isArray(v)) return v.filter(Boolean).map(String);
  return typeof v === 'string' && v.trim() ? [v] : [];
}

/** "Name <a@b>" → "Name"; a bare address stays as is. */
function displayName(address: string): string {
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
  const [open, setOpen] = useState<string | null>(null);

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
        const to = list(row.to);
        const first = to[0] ? displayName(to[0]) : t('scheduled.noRecipient');
        const failed = row.status === 'failed';
        const color = senderColor(to[0] || first);
        return (
          <div
            key={row.id}
            role="button"
            tabIndex={0}
            data-testid="scheduled-row"
            onClick={() => setOpen(row.id)}
            onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); setOpen(row.id); } }}
            className="scheduled-row"
            style={{ padding: 'var(--layout-row-py, 11px) var(--layout-row-px, 14px)', cursor: 'pointer', display: 'flex', gap: 10, alignItems: 'flex-start' }}
          >
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
      {open && <ScheduledPreview id={open} onClose={() => setOpen(null)} onCancelled={() => { setOpen(null); load(); }} />}
    </div>
  );
}

function ScheduledPreview({ id, onClose, onCancelled }: { id: string; onClose: () => void; onCancelled: () => void }) {
  const { t, i18n } = useTranslation();
  const locale = i18n.resolvedLanguage || i18n.language;
  const [mail, setMail] = useState<ScheduledDetail | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.getScheduledMail(id).then(setMail).catch(e => setError(toAppError(e).message));
  }, [id]);

  const cancel = async () => {
    setBusy(true);
    setError('');
    try {
      await api.cancelScheduled(id);
      onCancelled();
    } catch (e) {
      setError(toAppError(e).message);
      setBusy(false);
    }
  };

  const failed = mail?.status === 'failed';
  const field = (label: string, values: string[]) => values.length ? (
    <div style={{ display: 'flex', gap: 8, fontSize: 12.5 }}>
      <span style={{ width: 44, flexShrink: 0, color: 'var(--text-tertiary)' }}>{label}</span>
      <span style={{ minWidth: 0, overflowWrap: 'anywhere' }}>{values.join(', ')}</span>
    </div>
  ) : null;

  return (
    <Dialog
      title={mail?.subject || t('message.noSubject')}
      closeLabel={t('common.close')}
      onClose={onClose}
      busy={busy}
      testId="scheduled-preview"
      footer={<>
        <span style={{ fontSize: 12, color: error || failed ? 'var(--red)' : 'var(--text-secondary)', flex: 1, minWidth: 0 }}>
          {error || (failed ? (mail?.last_error || t('scheduled.failed')) : '')}
        </span>
        <div style={{ display: 'flex', gap: 8 }}>
          <Button onClick={onClose} disabled={busy}>{t('common.close')}</Button>
          <Button variant="danger" data-testid="scheduled-cancel" disabled={busy || !mail || mail.status === 'sending'} onClick={() => { void cancel(); }}>
            {failed ? t('scheduled.discard') : t('scheduled.cancelSend')}
          </Button>
        </div>
      </>}
    >
      {!mail ? (
        <div style={{ fontSize: 13, color: 'var(--text-tertiary)' }}>{error ? '' : t('common.loading')}</div>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
          <div style={{
            display: 'flex', alignItems: 'center', gap: 8, padding: '8px 11px', borderRadius: 8,
            background: failed ? 'color-mix(in srgb, var(--red) 12%, transparent)' : 'var(--accent-glow, var(--bg-tertiary))',
            color: failed ? 'var(--red)' : 'var(--accent)', fontSize: 12.5, fontWeight: 500,
          }}>
            <ClockIcon size={14} />
            {failed ? t('scheduled.failedAt', { when: formatScheduled(mail.send_at, locale) }) : t('scheduled.sendsAt', { when: formatScheduled(mail.send_at, locale) })}
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
            {field(t('compose.to'), list(mail.to))}
            {field(t('compose.cc'), list(mail.cc))}
            {field(t('compose.bcc'), list(mail.bcc))}
          </div>
          {!!mail.attachments?.length && (
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
              {mail.attachments.map((name, i) => (
                <span key={`${name}-${i}`} style={{ fontSize: 11.5, padding: '3px 8px', borderRadius: 6, background: 'var(--bg-tertiary)', color: 'var(--text-secondary)' }}>{name}</span>
              ))}
            </div>
          )}
          {mail.body_is_html ? (
            <div
              className="scheduled-preview-body"
              style={{ fontSize: 13, lineHeight: 1.55, borderTop: '1px solid var(--border-subtle)', paddingTop: 12, overflowWrap: 'anywhere' }}
              // The user's own composed HTML, sanitised like any rendered body.
              dangerouslySetInnerHTML={{ __html: DOMPurify.sanitize(mail.body || '') }}
            />
          ) : (
            <div style={{ fontSize: 13, lineHeight: 1.55, borderTop: '1px solid var(--border-subtle)', paddingTop: 12, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>
              {mail.body || ''}
            </div>
          )}
        </div>
      )}
    </Dialog>
  );
}
