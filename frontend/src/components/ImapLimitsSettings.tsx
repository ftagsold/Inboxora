// Admin: IMAP connection limits per mail server. Each account may open up to the server's
// start value of connections; refusals lower the account's limit automatically (remembered).
// After the mail server's connection settings change, an admin resets the learned limits.

import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties } from 'react';
import { useTranslation } from 'react-i18next';
import { api, type ImapLimitAccount, type ImapLimitServer, type ImapLimitsResponse } from '../utils/api.ts';
import { Button, inputStyle } from './ui.tsx';
import { formatDate } from '../utils/formatDate.ts';

const POLL_MS = 5000;

function serverKey(s: { host: string; port: number }) {
  return `${s.host}:${s.port}`;
}

export default function ImapLimitsSettings() {
  const { t } = useTranslation();
  const [data, setData] = useState<ImapLimitsResponse | null>(null);
  const [error, setError] = useState('');

  const load = useCallback(async () => {
    try {
      setData(await api.admin.imapLimits.list());
      setError('');
    } catch {
      setError(t('admin.imapLimits.loadFailed'));
    }
  }, [t]);

  useEffect(() => {
    void load();
    const id = setInterval(() => { if (document.visibilityState === 'visible') void load(); }, POLL_MS);
    return () => clearInterval(id);
  }, [load]);

  return (
    <div data-testid="imap-limits-settings" style={{ display: 'flex', flexDirection: 'column', gap: 16, maxWidth: 820 }}>
      <div>
        <h3 style={{ margin: '0 0 6px', fontSize: 15 }}>{t('admin.imapLimits.title')}</h3>
        <p style={{ margin: 0, fontSize: 12.5, color: 'var(--text-secondary)', lineHeight: 1.5 }}>{t('admin.imapLimits.help')}</p>
      </div>
      {error && <div role="alert" style={{ color: 'var(--red)', fontSize: 12.5 }}>{error}</div>}
      {data && data.servers.length === 0 && (
        <div style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('admin.imapLimits.empty')}</div>
      )}
      {data?.servers.map(server => (
        <ServerCard key={serverKey(server)} server={server} limits={data} onChanged={load} />
      ))}
    </div>
  );
}

function ServerCard({ server, limits, onChanged }: { server: ImapLimitServer; limits: ImapLimitsResponse; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const [start, setStart] = useState(server.startLimit == null ? '' : String(server.startLimit));
  const [autoLower, setAutoLower] = useState(server.autoLower);
  const [open, setOpen] = useState(false);
  const [confirmReset, setConfirmReset] = useState(false);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');
  const [error, setError] = useState('');

  // Follow server-side changes while the inputs are untouched.
  useEffect(() => { setStart(server.startLimit == null ? '' : String(server.startLimit)); }, [server.startLimit]);
  useEffect(() => { setAutoLower(server.autoLower); }, [server.autoLower]);

  const parsed = start.trim() === '' ? null : Number(start);
  const valid = parsed === null || (Number.isInteger(parsed) && parsed >= limits.min && parsed <= limits.max);
  const dirty = parsed !== server.startLimit || autoLower !== server.autoLower;

  const run = async (action: () => Promise<unknown>, done: string) => {
    setBusy(true); setError(''); setNotice('');
    try {
      await action();
      setNotice(done);
      await onChanged();
    } catch (e) {
      setError(e instanceof Error && e.message ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const save = () => run(() => api.admin.imapLimits.updateServer(server.host, server.port, { startLimit: parsed, autoLower }), t('admin.imapLimits.saved'));
  const reset = () => run(() => api.admin.imapLimits.resetServer(server.host, server.port), t('admin.imapLimits.resetDone')).finally(() => setConfirmReset(false));

  const totalOpen = server.accounts.reduce((n, a) => n + (a.live?.open || 0), 0);

  return (
    <section
      data-testid="imap-limits-server"
      style={{ border: '1px solid var(--border-subtle)', borderRadius: 10, padding: 14, background: 'var(--bg-elevated)' }}
    >
      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, alignItems: 'baseline', justifyContent: 'space-between' }}>
        <div style={{ fontWeight: 600, fontSize: 13.5, wordBreak: 'break-all' }}>{serverKey(server)}</div>
        <div style={{ fontSize: 12, color: 'var(--text-secondary)' }}>
          {t('admin.imapLimits.accounts', { count: server.accounts.length })} · {t('admin.imapLimits.openNow', { count: totalOpen })}
        </div>
      </div>

      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 14, alignItems: 'flex-end', marginTop: 12 }}>
        <label style={{ display: 'flex', flexDirection: 'column', gap: 5, fontSize: 12, color: 'var(--text-secondary)' }}>
          {t('admin.imapLimits.startLimit')}
          <input
            type="number"
            inputMode="numeric"
            min={limits.min}
            max={limits.max}
            value={start}
            placeholder={String(limits.defaultStartLimit)}
            onChange={e => setStart(e.target.value)}
            aria-invalid={!valid}
            style={{ ...inputStyle, width: 110 }}
          />
          <span style={{ fontSize: 11 }}>{t('admin.imapLimits.startLimitHint', { value: limits.defaultStartLimit, min: limits.min, max: limits.max })}</span>
        </label>
        <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 12.5, paddingBottom: 18, cursor: 'pointer' }}>
          <input type="checkbox" checked={autoLower} onChange={e => setAutoLower(e.target.checked)} />
          {t('admin.imapLimits.autoLower')}
        </label>
        <div style={{ display: 'flex', gap: 8, paddingBottom: 14, flexWrap: 'wrap' }}>
          <Button variant="primary" disabled={busy || !valid || !dirty} onClick={save}>{t('admin.imapLimits.save')}</Button>
          {confirmReset ? (
            <>
              <Button variant="danger" disabled={busy} onClick={reset}>{t('admin.imapLimits.resetConfirm')}</Button>
              <Button variant="ghost" disabled={busy} onClick={() => setConfirmReset(false)}>{t('admin.imapLimits.cancel')}</Button>
            </>
          ) : (
            <Button disabled={busy} onClick={() => setConfirmReset(true)}>{t('admin.imapLimits.resetServer')}</Button>
          )}
        </div>
      </div>
      {!valid && <div style={{ color: 'var(--red)', fontSize: 12 }}>{t('admin.imapLimits.invalid', { min: limits.min, max: limits.max })}</div>}
      {notice && <div role="status" style={{ color: 'var(--green, var(--accent))', fontSize: 12 }}>{notice}</div>}
      {error && <div role="alert" style={{ color: 'var(--red)', fontSize: 12 }}>{error}</div>}

      <button
        type="button"
        onClick={() => setOpen(o => !o)}
        aria-expanded={open}
        style={{ marginTop: 10, background: 'none', border: 'none', padding: 0, color: 'var(--accent)', fontSize: 12.5, cursor: 'pointer' }}
      >
        {open ? t('admin.imapLimits.hideAccounts') : t('admin.imapLimits.showAccounts')}
      </button>
      {open && (
        <div style={{ marginTop: 10, overflowX: 'auto' }}>
          <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 12.5, minWidth: 520 }}>
            <thead>
              <tr style={{ textAlign: 'left', color: 'var(--text-secondary)' }}>
                <th style={th}>{t('admin.imapLimits.colAccount')}</th>
                <th style={th}>{t('admin.imapLimits.colOwner')}</th>
                <th style={th}>{t('admin.imapLimits.colLimit')}</th>
                <th style={th}>{t('admin.imapLimits.colOpen')}</th>
                <th style={th}>{t('admin.imapLimits.colLastDrop')}</th>
                <th style={th} />
              </tr>
            </thead>
            <tbody>
              {server.accounts.map(a => <AccountRow key={a.id} account={a} server={server} onChanged={onChanged} />)}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

const th: CSSProperties = { padding: '6px 8px', fontWeight: 500, borderBottom: '1px solid var(--border-subtle)' };
const td: CSSProperties = { padding: '6px 8px', borderBottom: '1px solid var(--border-subtle)', verticalAlign: 'top' };

function AccountRow({ account, server, onChanged }: { account: ImapLimitAccount; server: ImapLimitServer; onChanged: () => Promise<void> }) {
  const { t, i18n } = useTranslation();
  const [busy, setBusy] = useState(false);
  const effective = account.live?.limit ?? (server.autoLower ? account.learnedLimit ?? server.effectiveStartLimit : server.effectiveStartLimit);
  const reset = async () => {
    setBusy(true);
    try {
      await api.admin.imapLimits.resetAccount(account.id);
      await onChanged();
    } finally {
      setBusy(false);
    }
  };
  return (
    <tr data-testid="imap-limits-account">
      <td style={{ ...td, wordBreak: 'break-all' }}>{account.email || account.id}</td>
      <td style={td}>{account.owner || '—'}</td>
      <td style={td}>
        {effective}
        {account.learnedLimit != null && <span style={{ color: 'var(--text-secondary)' }}> ({t('admin.imapLimits.learned')})</span>}
      </td>
      <td style={td}>
        {account.live ? `${account.live.open} / ${account.live.limit}` : '—'}
        {account.live && account.live.queued > 0 && <span style={{ color: 'var(--text-secondary)' }}> · {t('admin.imapLimits.queued', { count: account.live.queued })}</span>}
      </td>
      <td style={td}>
        {account.learnedAt ? formatDate(account.learnedAt, i18n.language) : '—'}
        {account.learnedReason && <div style={{ color: 'var(--text-secondary)', fontSize: 11.5 }}>{account.learnedReason}</div>}
      </td>
      <td style={{ ...td, textAlign: 'right' }}>
        <Button disabled={busy || account.learnedLimit == null} onClick={reset}>{t('admin.imapLimits.resetAccount')}</Button>
      </td>
    </tr>
  );
}
