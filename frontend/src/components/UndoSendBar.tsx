import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { useStore } from '../store/index.ts';
import { useMobile } from '../hooks/useMobile.ts';

// The undo-send window: a countdown bar while the sent mail waits, with Undo to bring the
// composer back. Leaving the page meanwhile would drop the mail, so the browser asks first.
export default function UndoSendBar() {
  const { t } = useTranslation();
  const pendingSend = useStore(state => state.pendingSend);
  const undoPendingSend = useStore(state => state.undoPendingSend);
  const isMobile = useMobile();

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
    <div style={{
      position: 'fixed', zIndex: 3001, left: 16, right: 16,
      bottom: isMobile ? 'calc(var(--sab) + 20px)' : 24,
      display: 'flex', justifyContent: 'center', pointerEvents: 'none',
    }}>
    <div
      role="status"
      aria-live="polite"
      data-testid="undo-send-bar"
      className="action-bar-enter"
      style={{
        position: 'relative', pointerEvents: 'all',
        width: isMobile ? '100%' : 380, maxWidth: '100%',
        background: 'var(--bg-elevated)',
        border: '1px solid var(--border)',
        borderRadius: 10,
        padding: '10px 8px 12px 16px',
        display: 'flex', alignItems: 'center', gap: 10,
        boxShadow: 'var(--shadow-soft)',
        overflow: 'hidden',
      }}
    >
      {!pendingSend.sending && <div
        key={pendingSend.id}
        style={{
          position: 'absolute', bottom: 0, left: 0, height: 3,
          background: 'var(--accent)',
          animation: `action-bar-progress ${pendingSend.seconds * 1000}ms linear forwards`,
        }}
      />}
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontSize: 13, fontWeight: 500, color: 'var(--text-primary)' }}>
          {pendingSend.sending ? t('compose.sending') : t('compose.undoSend.pending')}
        </div>
        <div style={{
          fontSize: 12, color: 'var(--text-tertiary)',
          overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
        }}>
          {pendingSend.subject}
        </div>
      </div>
      {!pendingSend.sending && <button
        onClick={undoPendingSend}
        data-testid="undo-send-button"
        style={{
          background: 'var(--accent-dim)',
          border: '1px solid var(--accent)',
          borderRadius: 6,
          color: 'var(--accent)',
          fontSize: 12, fontWeight: 600,
          padding: '5px 14px',
          cursor: 'pointer', flexShrink: 0,
        }}
      >
        {t('compose.undoSend.undo')}
      </button>}
    </div>
    </div>
  );
}
