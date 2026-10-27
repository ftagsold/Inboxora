import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { useStore } from '../store/index.ts';

// The undo-send window as a toast in the notification stack: a thin countdown bar while the
// sent mail waits, with Undo to bring the composer back. Leaving the page meanwhile would drop
// the mail, so the browser asks first.
export default function UndoSendToast({ isMobile }: { isMobile: boolean }) {
  const { t } = useTranslation();
  const pendingSend = useStore(state => state.pendingSend);
  const undoPendingSend = useStore(state => state.undoPendingSend);

  useEffect(() => {
    if (!pendingSend) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => { e.preventDefault(); };
    window.addEventListener('beforeunload', onBeforeUnload);
    return () => window.removeEventListener('beforeunload', onBeforeUnload);
  }, [pendingSend]);

  useEffect(() => {
    if (!pendingSend || pendingSend.sending) return;
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      // Text fields keep their own undo.
      if (target?.closest('input, textarea, [contenteditable="true"]')) return;
      if ((e.ctrlKey || e.metaKey) && !e.shiftKey && e.key.toLowerCase() === 'z') {
        e.preventDefault();
        undoPendingSend();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [pendingSend, undoPendingSend]);

  if (!pendingSend) return null;

  return (
    <div
      role="status"
      aria-live="polite"
      data-testid="undo-send-toast"
      className={isMobile ? 'toast-enter-mobile' : 'toast-enter'}
      style={{
        position: 'relative', overflow: 'hidden',
        background: 'var(--bg-elevated)', border: '1px solid var(--border)',
        borderRadius: 10, padding: '12px 14px',
        display: 'flex', alignItems: 'center', gap: 10,
        width: isMobile ? '100%' : 300, maxWidth: isMobile ? '100%' : 340,
        boxShadow: 'var(--shadow-popover)',
        pointerEvents: 'all',
      }}
    >
      {!pendingSend.sending && <div
        key={pendingSend.id}
        style={{
          position: 'absolute', bottom: 0, left: 0, height: 2,
          background: 'var(--accent)',
          animation: `action-bar-progress ${pendingSend.seconds * 1000}ms linear forwards`,
        }}
      />}
      <div style={{
        width: 32, height: 32, borderRadius: 8, flexShrink: 0,
        background: 'var(--accent-dim)', color: 'var(--accent)',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
      }}>
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/>
        </svg>
      </div>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontSize: 13, fontWeight: 500, color: 'var(--text-primary)', marginBottom: 2 }}>
          {pendingSend.sending ? t('compose.sending') : t('compose.undoSend.pending')}
        </div>
        <div style={{ fontSize: 12, color: 'var(--text-tertiary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {pendingSend.subject}
        </div>
      </div>
      {!pendingSend.sending && <button
        onClick={undoPendingSend}
        data-testid="undo-send-button"
        style={{
          background: 'var(--accent-dim)', border: '1px solid rgba(124,106,247,0.3)',
          borderRadius: 5, color: 'var(--accent)', fontSize: 11, fontWeight: 600,
          padding: '3px 10px', cursor: 'pointer', flexShrink: 0,
          transition: 'background 0.12s',
        }}
        onMouseEnter={e => { e.currentTarget.style.background = 'rgba(124,106,247,0.25)'; }}
        onMouseLeave={e => { e.currentTarget.style.background = 'var(--accent-dim)'; }}
      >
        {t('common.undo')}
      </button>}
    </div>
  );
}
