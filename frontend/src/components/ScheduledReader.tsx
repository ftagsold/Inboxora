// A scheduled (not yet sent) mail in the reader pane. The toolbar reschedules it (clock),
// opens it in the composer for editing (Save replaces Send there), or deletes it, which also
// removes the schedule. Once the mail goes out the row disappears and the reader closes.

import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import DOMPurify from 'dompurify';
import { api } from '../utils/api.ts';
import { useStore } from '../store/index.ts';
import type { ComposeDraft } from '../store/index.ts';
import { formatScheduled } from '../utils/scheduleSend.ts';
import { toAppError } from '../utils/errors.ts';
import { ToolbarButton } from './MessageToolbar.tsx';
import ScheduleSendDialog from './ScheduleSendDialog.tsx';
import { ClockIcon, recipientList, type ScheduledRow } from './ScheduledSection.tsx';

interface ScheduledDetail extends ScheduledRow {
  body?: string | null;
  body_is_html?: boolean;
  payload?: Record<string, unknown>;
}

const icon = { width: 15, height: 15, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', strokeWidth: 2, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const };

/** The stored send request as the composer's starting state. */
function composeFromPayload(id: string, p: Record<string, unknown>): ComposeDraft {
  const str = (k: string) => (typeof p[k] === 'string' ? p[k] as string : undefined);
  const list = (k: string) => (Array.isArray(p[k]) ? (p[k] as unknown[]).map(String) : typeof p[k] === 'string' ? [p[k] as string] : []);
  const uploads = Array.isArray(p.attachments) ? p.attachments as Array<Record<string, unknown>> : [];
  const forwarded = Array.isArray(p.forwardedAttachments) ? p.forwardedAttachments as Array<Record<string, unknown>> : [];
  return {
    scheduledId: id,
    accountId: str('accountId'),
    aliasId: str('aliasId') ?? null,
    to: list('to'),
    cc: list('cc'),
    bcc: list('bcc'),
    subject: str('subject') ?? '',
    body: str('body') ?? '',
    bodyIsHtml: p.bodyIsHtml !== false,
    quotedBody: str('quotedBody'),
    quotedBodyHtml: str('quotedBodyHtml') ?? null,
    ...(p.editedSignature !== undefined ? { editedSignature: str('editedSignature') ?? '', editedSignatureIsHtml: p.editedSignatureIsHtml !== false } : {}),
    inReplyTo: str('inReplyTo') ?? null,
    references: str('references') ?? null,
    priority: str('priority'),
    attachments: uploads.map(a => {
      const data = String(a.content || '');
      return { name: String(a.filename || 'attachment'), data, type: String(a.contentType || 'application/octet-stream'), size: Math.floor(data.length * 3 / 4) };
    }),
    forwardedAttachments: forwarded.map(a => ({ messageId: String(a.messageId || ''), part: String(a.part || ''), filename: typeof a.filename === 'string' ? a.filename : null })),
  };
}

export default function ScheduledReader({ id, onBack }: { id: string; onBack?: () => void }) {
  const { t, i18n } = useTranslation();
  const locale = i18n.resolvedLanguage || i18n.language;
  const setSelectedScheduled = useStore(s => s.setSelectedScheduled);
  const openCompose = useStore(s => s.openCompose);
  const [mail, setMail] = useState<ScheduledDetail | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [rescheduling, setRescheduling] = useState(false);

  const load = useCallback(() => {
    api.getScheduledMail(id)
      .then((m: ScheduledDetail) => { setMail(m); setError(''); })
      .catch(e => {
        // Gone: it was sent (or deleted elsewhere). Close the reader.
        if (toAppError(e).status === 404) setSelectedScheduled(null);
        else setError(toAppError(e).message);
      });
  }, [id, setSelectedScheduled]);

  useEffect(() => {
    setMail(null);
    load();
    const onChange = () => load();
    window.addEventListener('inboxora:scheduled_changed', onChange);
    return () => window.removeEventListener('inboxora:scheduled_changed', onChange);
  }, [load]);

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    setError('');
    try {
      await action();
      return true;
    } catch (e) {
      setError(toAppError(e).message);
      return false;
    } finally {
      setBusy(false);
    }
  };

  const remove = () => run(async () => {
    await api.cancelScheduled(id);
    setSelectedScheduled(null);
  });

  const reschedule = async (at: Date) => {
    if (await run(() => api.updateScheduled(id, { sendAt: at.toISOString() }))) {
      setRescheduling(false);
      load();
    }
  };

  const edit = () => run(async () => {
    const full: ScheduledDetail = await api.getScheduledMail(id, true);
    if (full.payload) openCompose(composeFromPayload(id, full.payload));
  });

  const failed = mail?.status === 'failed';
  const sending = mail?.status === 'sending';
  const field = (label: string, values: string[]) => values.length ? (
    <div style={{ display: 'flex', gap: 10, fontSize: 12.5, lineHeight: 1.5 }}>
      <span style={{ width: 40, flexShrink: 0, color: 'var(--text-tertiary)' }}>{label}</span>
      <span style={{ minWidth: 0, overflowWrap: 'anywhere', color: 'var(--text-secondary)' }}>{values.join(', ')}</span>
    </div>
  ) : null;

  return (
    <div data-testid="scheduled-reader" style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', height: '100%', background: 'var(--bg-primary)' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 2, padding: '8px 12px', borderBottom: '1px solid var(--border-subtle)', flexShrink: 0 }}>
        {onBack && (
          <ToolbarButton onClick={onBack} title={t('common.back')}>
            <svg {...icon}><polyline points="15 18 9 12 15 6" /></svg>
          </ToolbarButton>
        )}
        <ToolbarButton onClick={() => { if (!busy && !sending) setRescheduling(true); }} title={t('scheduled.reschedule')} action="scheduled-reschedule">
          <ClockIcon size={15} /> <span>{t('scheduled.reschedule')}</span>
        </ToolbarButton>
        <ToolbarButton onClick={() => { if (!busy && !sending) void edit(); }} title={t('scheduled.edit')} action="scheduled-edit">
          <svg {...icon}><path d="M12 20h9" /><path d="M16.5 3.5a2.121 2.121 0 013 3L7 19l-4 1 1-4L16.5 3.5z" /></svg> <span>{t('scheduled.edit')}</span>
        </ToolbarButton>
        <div style={{ flex: 1 }} />
        <ToolbarButton danger onClick={() => { if (!busy && !sending) void remove(); }} title={t('scheduled.delete')} action="scheduled-delete">
          <svg {...icon}><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a1 1 0 011-1h4a1 1 0 011 1v2" /></svg>
        </ToolbarButton>
      </div>

      <div style={{ flex: 1, overflowY: 'auto', padding: '20px 24px 32px' }}>
        {error && <div role="alert" style={{ marginBottom: 12, fontSize: 12.5, color: 'var(--red)' }}>{error}</div>}
        {!mail ? (
          !error && <div style={{ fontSize: 13, color: 'var(--text-tertiary)' }}>{t('common.loading')}</div>
        ) : (
          <div style={{ maxWidth: 760, display: 'flex', flexDirection: 'column', gap: 14 }}>
            <h1 style={{ margin: 0, fontFamily: 'var(--font-display)', fontSize: 20, fontWeight: 600, color: 'var(--text-max-contrast)', overflowWrap: 'anywhere' }}>
              {mail.subject || t('message.noSubject')}
            </h1>
            <button
              type="button"
              onClick={() => { if (!busy && !sending) setRescheduling(true); }}
              style={{
                display: 'flex', alignItems: 'center', gap: 8, padding: '9px 12px', borderRadius: 8, border: 'none', textAlign: 'left',
                background: failed ? 'color-mix(in srgb, var(--red) 12%, transparent)' : 'var(--accent-dim, var(--bg-tertiary))',
                color: failed ? 'var(--red)' : 'var(--accent)', fontSize: 13, fontWeight: 500, cursor: sending ? 'default' : 'pointer',
              }}
            >
              <ClockIcon size={15} />
              <span style={{ flex: 1 }}>
                {sending ? t('scheduled.sending')
                  : failed ? `${t('scheduled.failedAt', { when: formatScheduled(mail.send_at, locale) })}${mail.last_error ? ` · ${mail.last_error}` : ''}`
                    : t('scheduled.sendsAt', { when: formatScheduled(mail.send_at, locale) })}
              </span>
            </button>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
              {field(t('compose.to'), recipientList(mail.to))}
              {field(t('compose.cc'), recipientList(mail.cc))}
              {field(t('compose.bcc'), recipientList(mail.bcc))}
            </div>
            {!!mail.attachments?.length && (
              <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                {mail.attachments.map((name, i) => (
                  <span key={`${name}-${i}`} style={{ fontSize: 11.5, padding: '4px 9px', borderRadius: 6, background: 'var(--bg-tertiary)', color: 'var(--text-secondary)' }}>{name}</span>
                ))}
              </div>
            )}
            <div style={{ borderTop: '1px solid var(--border-subtle)', paddingTop: 16, fontSize: 14, lineHeight: 1.6, color: 'var(--text-primary)', overflowWrap: 'anywhere', ...(mail.body_is_html ? {} : { whiteSpace: 'pre-wrap' as const }) }}>
              {mail.body_is_html
                // The user's own composed HTML, sanitised like any rendered body.
                ? <div className="scheduled-reader-body" dangerouslySetInnerHTML={{ __html: DOMPurify.sanitize(mail.body || '') }} />
                : (mail.body || '')}
            </div>
          </div>
        )}
      </div>

      {rescheduling && mail && (
        <ScheduleSendDialog
          initial={new Date(mail.send_at)}
          busy={busy}
          onClose={() => setRescheduling(false)}
          onConfirm={at => { void reschedule(at); }}
        />
      )}
    </div>
  );
}
