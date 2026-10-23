// Full-screen "your account is ready shortly" screen. After a Redis or server restart (and for
// a newly added account) the server loads the display fields of every folder into its cache;
// the mail UI stays blocked until that finishes so lists open complete and fast. Progress comes
// from `meta_preload` WebSocket events, with GET /mail/preload/status polled as a fallback.

import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api, type PreloadAccountStatus, type PreloadStatus } from '../utils/api.ts';
import { applyPreloadEvent, overallProgress, type PreloadEventDetail } from '../utils/preloadProgress.ts';
import LogoMark from './LogoMark.tsx';

const POLL_MS = 3000;
/** A preload that starts while the app is open and finishes this fast never shows the screen
 * (no flash on a warm cache). At sign-in the screen shows at once instead. */
const SHOW_AFTER_MS = 800;

export default function AccountPreparing() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<PreloadStatus | null>(null);
  const [visible, setVisible] = useState(false);
  const shownRef = useRef(false);
  // Until the first status arrives the mail app stays covered, so an account that is still
  // being prepared at sign-in never flashes the app before the screen.
  const [initial, setInitial] = useState(true);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.getPreloadStatus());
    } catch {
      // Keep the last known state; a failing status endpoint must not block the app forever.
      setStatus(prev => prev ?? { ready: true, accounts: [] });
    }
  }, []);

  useEffect(() => { void refresh(); }, [refresh]);

  // Poll only while blocked.
  useEffect(() => {
    if (!status || status.ready) return undefined;
    const id = setInterval(() => { void refresh(); }, POLL_MS);
    return () => clearInterval(id);
  }, [status, refresh]);

  useEffect(() => {
    const onEvent = (e: Event) => {
      const detail = (e as CustomEvent<PreloadEventDetail>).detail || {};
      // A new cold preload (Redis restarted, account added): block again.
      if (detail.state === 'running') {
        setStatus(prev => prev && prev.accounts.some(a => a.id === detail.accountId) ? applyPreloadEvent({ ...prev, ready: false }, detail) : prev);
        void refresh();
        return;
      }
      setStatus(prev => prev ? applyPreloadEvent(prev, detail) : prev);
    };
    window.addEventListener('inboxora:meta_preload', onEvent);
    return () => window.removeEventListener('inboxora:meta_preload', onEvent);
  }, [refresh]);

  const blocked = !!status && !status.ready;
  useEffect(() => {
    if (!blocked) {
      setVisible(false);
      if (shownRef.current) {
        shownRef.current = false;
        // The cache is complete now: reload the list so every row has its fields.
        window.dispatchEvent(new CustomEvent('inboxora:refresh'));
      }
      return undefined;
    }
    if (initial) {
      shownRef.current = true;
      return undefined;
    }
    const id = setTimeout(() => { shownRef.current = true; setVisible(true); }, SHOW_AFTER_MS);
    return () => clearTimeout(id);
  }, [blocked, initial]);
  // Once the first answer is in, later preloads (account added, Redis restarted) show the
  // screen only when they take longer than SHOW_AFTER_MS.
  useEffect(() => { if (status && !blocked) setInitial(false); }, [status, blocked]);

  if (!status) {
    return <div data-testid="account-preparing-pending" aria-hidden="true" style={{ position: 'fixed', inset: 0, zIndex: 9500, background: 'var(--bg-primary)' }} />;
  }
  if (!blocked || !(visible || initial)) return null;
  const progress = overallProgress(status.accounts);
  const pct = progress === null ? null : Math.round(progress * 100);

  return (
    <div
      data-testid="account-preparing"
      role="dialog"
      aria-modal="true"
      aria-busy="true"
      aria-labelledby="account-preparing-title"
      style={{
        position: 'fixed', inset: 0, zIndex: 9500, display: 'flex', alignItems: 'center', justifyContent: 'center',
        padding: 16, background: 'var(--bg-primary)', color: 'var(--text-primary)',
      }}
    >
      <div style={{
        width: '100%', maxWidth: 440, boxSizing: 'border-box', padding: '28px 24px', borderRadius: 14,
        background: 'var(--bg-elevated)', border: '1px solid var(--border-subtle)',
        boxShadow: '0 12px 40px rgba(0,0,0,0.12)', display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 14,
      }}>
        <LogoMark size={44} />
        <h1 id="account-preparing-title" style={{ margin: 0, fontSize: 19, fontWeight: 650, textAlign: 'center' }}>
          {t('preload.title')}
        </h1>
        <p style={{ margin: 0, fontSize: 13, lineHeight: 1.5, color: 'var(--text-secondary)', textAlign: 'center' }}>
          {t('preload.subtitle')}
        </p>
        <ProgressBar value={progress} label={pct === null ? t('preload.waiting') : t('preload.percent', { value: pct })} />
        {status.accounts.length > 1 || status.accounts.some(a => a.state === 'failed') ? (
          <ul style={{ listStyle: 'none', margin: 0, padding: 0, width: '100%', display: 'flex', flexDirection: 'column', gap: 10 }}>
            {status.accounts.map(a => <AccountLine key={a.id} account={a} />)}
          </ul>
        ) : status.accounts[0] ? (
          <div style={{ fontSize: 12, color: 'var(--text-secondary)', minHeight: 16, textAlign: 'center' }}>{lineText(t, status.accounts[0])}</div>
        ) : null}
      </div>
    </div>
  );
}

function lineText(t: (k: string, o?: Record<string, unknown>) => string, a: PreloadAccountStatus): string {
  if (a.state === 'failed') return t('preload.failed');
  if (a.state === 'done') return t('preload.done');
  if (!a.total) return t('preload.waiting');
  // One bar for the whole account: the size, not a per-phase count that restarts.
  const messages = a.messages || a.total;
  const count = t('preload.messages', { count: messages, total: messages.toLocaleString() });
  return a.folder ? `${count} · ${t('preload.folder', { folder: a.folder })}` : count;
}

function AccountLine({ account }: { account: PreloadAccountStatus }) {
  const { t } = useTranslation();
  const value = account.state === 'done' ? 1 : account.state === 'failed' ? 0 : account.total ? account.done / account.total : null;
  return (
    <li style={{ display: 'flex', flexDirection: 'column', gap: 5 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8, fontSize: 12.5 }}>
        <span style={{ fontWeight: 550, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{account.email || account.id}</span>
      </div>
      <ProgressBar value={value} small failed={account.state === 'failed'} label={lineText(t, account)} />
      {account.state === 'failed' && account.error && (
        <div style={{ fontSize: 11.5, color: 'var(--red)', overflowWrap: 'anywhere' }}>{account.error}</div>
      )}
    </li>
  );
}

function ProgressBar({ value, label, small = false, failed = false }: { value: number | null; label: string; small?: boolean; failed?: boolean }) {
  const height = small ? 5 : 8;
  return (
    <div style={{ width: '100%', display: 'flex', flexDirection: 'column', gap: 6 }}>
      <div
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={value === null ? undefined : Math.round(value * 100)}
        style={{ position: 'relative', width: '100%', height, borderRadius: height, background: 'var(--border-subtle)', overflow: 'hidden' }}
      >
        {value === null ? (
          <div className="preload-bar-indeterminate" style={{ position: 'absolute', top: 0, bottom: 0, width: '35%', borderRadius: height, background: 'var(--accent)' }} />
        ) : (
          <div style={{
            height: '100%', width: `${Math.max(failed ? 0 : 2, value * 100)}%`, borderRadius: height,
            background: failed ? 'var(--red)' : 'var(--accent)', transition: 'width 0.5s ease',
          }} />
        )}
      </div>
      <div style={{ fontSize: small ? 11.5 : 12, color: 'var(--text-secondary)', textAlign: small ? 'left' : 'center' }}>{label}</div>
    </div>
  );
}
