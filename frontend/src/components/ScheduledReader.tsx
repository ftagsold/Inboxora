// A scheduled (not yet sent) mail in the reader pane, drawn as the same card the conversation
// reader uses (subject, sender, recipients, date, body), with a toolbar fitted to it: the clock
// reschedules, Edit opens the composer (Save replaces Send there), Delete removes the mail and
// its schedule. Once the mail goes out the row disappears and the reader closes.

import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import DOMPurify from 'dompurify';
import { api } from '../utils/api.ts';
import { useStore } from '../store/index.ts';
import type { ComposeDraft } from '../store/index.ts';
import { formatScheduled } from '../utils/scheduleSend.ts';
import { toAppError } from '../utils/errors.ts';
import { ToolbarButton } from './MessageToolbar.tsx';
import MessageDetailContent from './MessageDetailContent.tsx';
import { MessageAvatar, MessageDirection } from './MessagePresentation.tsx';
import { useMobile } from '../hooks/useMobile.ts';
import ScheduleSendDialog from './ScheduleSendDialog.tsx';
import { ClockIcon, displayName, recipientList, type ScheduledRow } from './ScheduledSection.tsx';

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
  const isMobile = useMobile();
  const setSelectedScheduled = useStore(s => s.setSelectedScheduled);
  const openCompose = useStore(s => s.openCompose);
  const accounts = useStore(s => s.accounts);
  const [mail, setMail] = useState<ScheduledDetail | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [rescheduling, setRescheduling] = useState(false);
  const [expanded, setExpanded] = useState(true);

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
    setExpanded(true);
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

  const account = accounts.find(a => a.id === mail?.account_id);
  const accountColor = account?.color || 'var(--accent)';
  const senderName = (typeof account?.sender_name === 'string' && account.sender_name) || account?.name || account?.email_address || '';
  const senderEmail = account?.email_address || '';
  const failed = mail?.status === 'failed';
  const sending = mail?.status === 'sending';
  const locked = busy || sending;
  const to = recipientList(mail?.to).join(', ');
  const cc = recipientList(mail?.cc).join(', ');
  const bcc = recipientList(mail?.bcc).join(', ');
  const when = mail ? formatScheduled(mail.send_at, locale) : '';
  const firstRecipient = recipientList(mail?.to)[0];

  const toolbar = (
    <div data-testid="scheduled-toolbar" onClick={e => e.stopPropagation()} style={{
      padding: '8px 16px', borderBottom: '1px solid var(--border-subtle)', display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap', minWidth: 0,
    }}>
      {onBack && (
        <ToolbarButton onClick={onBack} title={t('common.back')}>
          <svg {...icon}><polyline points="15 18 9 12 15 6" /></svg>
        </ToolbarButton>
      )}
      <ToolbarButton onClick={() => { if (!locked) setRescheduling(true); }} title={t('scheduled.reschedule')} action="scheduled-reschedule">
        <ClockIcon size={15} />
      </ToolbarButton>
      <ToolbarButton onClick={() => { if (!locked) void edit(); }} title={t('scheduled.edit')} action="scheduled-edit">
        <svg {...icon}><path d="M12 20h9" /><path d="M16.5 3.5a2.121 2.121 0 013 3L7 19l-4 1 1-4L16.5 3.5z" /></svg>
      </ToolbarButton>
      <ToolbarButton danger onClick={() => { if (!locked) void remove(); }} title={t('scheduled.delete')} action="scheduled-delete">
        <svg {...icon}><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a1 1 0 011-1h4a1 1 0 011 1v2" /></svg>
      </ToolbarButton>
      <span style={{
        marginLeft: 'auto', display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: 12, fontWeight: 500,
        color: failed ? 'var(--red)' : 'var(--accent)', minWidth: 0,
      }}>
        <ClockIcon size={13} />
        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {sending ? t('scheduled.sending') : failed ? t('scheduled.failedAt', { when }) : t('scheduled.sendsAt', { when })}
        </span>
      </span>
    </div>
  );

  return (
    <div data-testid="scheduled-reader" style={{ flex: 1, minWidth: 0, height: '100%', overflowY: 'auto', overflowX: 'hidden', background: 'var(--bg-primary)', paddingTop: isMobile ? 0 : 2 }}>
      {error && <div role="alert" style={{ margin: isMobile ? 12 : '12px 14px 0', fontSize: 12.5, color: 'var(--red)' }}>{error}</div>}
      {!mail ? (
        !error && <div style={{ padding: isMobile ? 12 : '14px 16px', fontSize: 13, color: 'var(--text-tertiary)' }}>{t('common.loading')}</div>
      ) : (
        <article data-conversation-message-state={expanded ? 'expanded' : 'collapsed'} style={{ padding: isMobile ? '12px 0 0' : '12px 14px 14px', background: 'var(--bg-primary)' }}>
          <div className="msg-card" style={{
            background: 'var(--bg-elevated)', borderRadius: 10, border: '1px solid var(--border-subtle)',
            borderLeft: `3px solid ${accountColor}`, overflow: 'visible', position: 'relative',
            boxShadow: 'var(--shadow-soft), inset 0 1px 0 rgba(255,255,255,0.04)',
          }}>
            {expanded && toolbar}
            <div
              role="button"
              tabIndex={0}
              aria-expanded={expanded}
              onClick={() => { const sel = window.getSelection(); if (sel !== null && sel.toString()) return; setExpanded(v => !v); }}
              onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); setExpanded(v => !v); } }}
              style={{ display: 'block', width: '100%', cursor: 'pointer', textAlign: 'left', userSelect: 'text', color: 'var(--text-primary)' }}
            >
              <span style={{ display: 'block', padding: '14px 16px 12px', borderBottom: '1px solid var(--border-subtle)', fontSize: 19, fontWeight: 600, lineHeight: 1.3, fontFamily: 'var(--font-display)' }}>
                {mail.subject || t('message.noSubject')}
              </span>
              <span style={{ display: 'flex', alignItems: 'flex-start', gap: 12, padding: '12px 16px' }}>
                <MessageAvatar email={senderEmail} name={senderName} size={40} />
                <span style={{ flex: 1, minWidth: 0 }}>
                  <span style={{ display: 'flex', alignItems: 'baseline', gap: 8, flexWrap: 'wrap' }}>
                    <MessageDirection direction="outgoing" />
                    <strong style={{ fontSize: 14, color: 'var(--text-primary)' }}>{senderName}</strong>
                    {senderEmail && <span style={{ fontSize: 12, color: 'var(--text-tertiary)' }}>{senderEmail}</span>}
                  </span>
                  <span style={{ display: 'block', fontSize: 12, color: 'var(--text-tertiary)', marginTop: 3 }}>
                    <span>{t('message.to')} </span>
                    <span style={{ color: 'var(--text-secondary)' }}>{expanded ? to : (firstRecipient ? displayName(firstRecipient) : '')}</span>
                  </span>
                  {expanded && cc && <span style={{ display: 'block', fontSize: 12, color: 'var(--text-tertiary)', marginTop: 3 }}>
                    <span>{t('message.cc', { defaultValue: 'Cc' })} </span><span style={{ color: 'var(--text-secondary)' }}>{cc}</span>
                  </span>}
                  {expanded && bcc && <span style={{ display: 'block', fontSize: 12, color: 'var(--text-tertiary)', marginTop: 3 }}>
                    <span>{t('compose.bcc')} </span><span style={{ color: 'var(--text-secondary)' }}>{bcc}</span>
                  </span>}
                  {failed && mail.last_error && <span role="alert" style={{ display: 'block', fontSize: 12, color: 'var(--red)', marginTop: 3 }}>{mail.last_error}</span>}
                </span>
                <span style={{ flexShrink: 0, textAlign: 'right' }}>
                  <span style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 12, color: failed ? 'var(--red)' : 'var(--accent)', whiteSpace: 'nowrap' }}>
                    <ClockIcon size={12} />{when}
                  </span>
                  {account && <span style={{ fontSize: 11, marginTop: 4, display: 'flex', alignItems: 'center', gap: 4, justifyContent: 'flex-end', color: 'var(--text-tertiary)' }}>
                    <span aria-hidden="true" style={{ width: 6, height: 6, borderRadius: '50%', background: accountColor }} />
                    {account.name || account.email_address}
                  </span>}
                </span>
              </span>
            </div>
            {expanded && <MessageDetailContent
              message={{ id, account_id: mail.account_id, from_email: senderEmail, from_name: senderName }}
              body={{
                html: mail.body_is_html ? DOMPurify.sanitize(mail.body || '') : null,
                text: mail.body_is_html ? null : (mail.body || ''),
                attachments: (mail.attachments || []).map((filename, i) => ({ part: `scheduled-${i}`, filename })),
              }}
              canAccessCopy={false}
              mobile={isMobile}
              embedded
            />}
          </div>
        </article>
      )}

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
