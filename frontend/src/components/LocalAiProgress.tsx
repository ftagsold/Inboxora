import { useTranslation } from 'react-i18next';
import { useLocalAiState, useLocalAiSupport } from '../hooks/useLocalAiState.ts';

// While the in-browser model downloads or loads, a small bar shows how far along it is, so the
// first AI request after enabling (or on a new device) does not look stuck.
export default function LocalAiProgress() {
  const { t } = useTranslation();
  const state = useLocalAiState();
  // Mounted with the app, so the device probe runs in the background from the start.
  useLocalAiSupport();
  if (state.status !== 'loading') return null;
  const percent = Math.round(state.progress * 100);
  return (
    <div
      role="status"
      data-testid="local-ai-progress"
      style={{
        position: 'fixed', left: 16, bottom: 'calc(var(--sab) + 16px)', zIndex: 2999,
        width: 260, maxWidth: 'calc(100vw - 32px)',
        background: 'var(--bg-elevated)', border: '1px solid var(--border)',
        borderRadius: 10, padding: '10px 14px', boxShadow: 'var(--shadow-soft)',
        fontSize: 12, color: 'var(--text-secondary)',
      }}
    >
      <div style={{ marginBottom: 6 }}>{t('admin.localAi.loading', { percent })}</div>
      <LocalAiProgressBar progress={state.progress} />
    </div>
  );
}

export function LocalAiProgressBar({ progress }: { progress: number }) {
  return (
    <div style={{ height: 4, borderRadius: 2, background: 'var(--bg-tertiary)', overflow: 'hidden' }}>
      <div style={{
        height: '100%', width: `${Math.max(0, Math.min(1, progress)) * 100}%`,
        background: 'var(--accent)', transition: 'width 0.3s ease',
      }} />
    </div>
  );
}
