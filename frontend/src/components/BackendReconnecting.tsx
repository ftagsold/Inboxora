// Blocks the UI while the Inboxora backend is unreachable. The screen waits a moment before
// it appears so a short blip (a restart, a deploy) never flashes it, and it disappears as
// soon as the backend answers again.

import { useTranslation } from 'react-i18next';
import { useBackendDown } from '../utils/backendStatus.ts';
import { useDelayedFlag } from '../hooks/useDelayedFlag.ts';
import LogoMark from './LogoMark.tsx';

const SHOW_AFTER_MS = 1200;

export default function BackendReconnecting() {
  const { t } = useTranslation();
  const visible = useDelayedFlag(useBackendDown(), SHOW_AFTER_MS);
  if (!visible) return null;

  return (
    <div
      className="backend-reconnecting"
      data-testid="backend-reconnecting"
      role="alertdialog"
      aria-modal="true"
      aria-busy="true"
      aria-labelledby="backend-reconnecting-title"
      aria-describedby="backend-reconnecting-body"
      style={{
        position: 'fixed', inset: 0, zIndex: 9700, display: 'flex', alignItems: 'center', justifyContent: 'center',
        padding: 16, color: 'var(--text-primary)',
      }}
    >
      <div style={{
        width: '100%', maxWidth: 360, boxSizing: 'border-box', padding: '28px 24px', borderRadius: 14,
        background: 'var(--bg-elevated)', border: '1px solid var(--border-subtle)',
        boxShadow: '0 12px 40px rgba(0,0,0,0.18)', display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 12,
      }}>
        <div className="backend-reconnecting-mark"><LogoMark size={40} /></div>
        <h1 id="backend-reconnecting-title" style={{ margin: '4px 0 0', fontSize: 17, fontWeight: 650, textAlign: 'center' }}>
          {t('backendReconnecting.title')}
        </h1>
        <p id="backend-reconnecting-body" style={{ margin: 0, fontSize: 13, lineHeight: 1.5, color: 'var(--text-secondary)', textAlign: 'center' }}>
          {t('backendReconnecting.body')}
        </p>
        <div className="backend-reconnecting-dots" aria-hidden="true"><span /><span /><span /></div>
      </div>
    </div>
  );
}
