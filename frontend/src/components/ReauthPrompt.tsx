// Sends the user through SSO sign-in again when the server reports that an OIDC-linked
// mailbox's sign-in is no longer accepted (`reauth_required`: refresh token rejected, or the
// mail server refuses a freshly refreshed token). Signing in again stores new tokens for the
// mailbox and reconnects it. With an auto-login provider configured the redirect happens on its
// own (at most once per REDIRECT_COOLDOWN_MS, so a sign-in that does not fix it cannot loop);
// otherwise, or after that, a small dialog offers it.

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useStore } from '../store/index.ts';
import { api } from '../utils/api.ts';

const REDIRECT_KEY = 'inboxora_reauth_redirect_at';
const REDIRECT_COOLDOWN_MS = 10 * 60 * 1000;

interface ProviderList { providers?: Array<{ id: string; slug: string; name?: string }>; autoLoginSlug?: string | null }

function recentlyRedirected(): boolean {
  try {
    return Date.now() - Number(sessionStorage.getItem(REDIRECT_KEY) || 0) < REDIRECT_COOLDOWN_MS;
  } catch {
    return false;
  }
}

function startSignIn(slug: string): void {
  try { sessionStorage.setItem(REDIRECT_KEY, String(Date.now())); } catch { /* best effort */ }
  window.location.href = `/auth/oidc/${encodeURIComponent(slug)}/start`;
}

export default function ReauthPrompt() {
  const { t } = useTranslation();
  const account = useStore(s => s.accounts.find(a => a.enabled !== false && a.reauth_required && a.oidc_provider_id));
  const [providers, setProviders] = useState<ProviderList | null>(null);
  const [dismissed, setDismissed] = useState<string | null>(null);

  const needed = !!account;
  useEffect(() => {
    if (!needed || providers) return;
    api.oidc.getProviders().then((data: ProviderList) => setProviders(data)).catch(() => setProviders({}));
  }, [needed, providers]);

  const provider = account && providers?.providers?.find(p => p.id === account.oidc_provider_id);
  const auto = !!provider && providers?.autoLoginSlug === provider.slug && !recentlyRedirected();

  useEffect(() => {
    if (auto && provider) startSignIn(provider.slug);
  }, [auto, provider]);

  if (!account || !providers || auto || dismissed === account.id) return null;

  return (
    <div
      data-testid="reauth-prompt"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="reauth-prompt-title"
      aria-describedby="reauth-prompt-body"
      className="backend-reconnecting"
      style={{
        position: 'fixed', inset: 0, zIndex: 9600, display: 'flex', alignItems: 'center', justifyContent: 'center',
        padding: 16, color: 'var(--text-primary)',
      }}
    >
      <div style={{
        width: '100%', maxWidth: 380, boxSizing: 'border-box', padding: '24px 22px', borderRadius: 14,
        background: 'var(--bg-elevated)', border: '1px solid var(--border-subtle)',
        boxShadow: '0 12px 40px rgba(0,0,0,0.18)', display: 'flex', flexDirection: 'column', gap: 12,
      }}>
        <h1 id="reauth-prompt-title" style={{ margin: 0, fontSize: 17, fontWeight: 650 }}>{t('reauth.title')}</h1>
        <p id="reauth-prompt-body" style={{ margin: 0, fontSize: 13, lineHeight: 1.5, color: 'var(--text-secondary)' }}>
          {t('reauth.body', { email: account.email_address || account.name || '' })}
        </p>
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, marginTop: 4 }}>
          <button
            type="button"
            onClick={() => setDismissed(account.id)}
            style={{ padding: '7px 14px', borderRadius: 7, fontSize: 13, cursor: 'pointer', border: '1px solid var(--border)', background: 'var(--bg-tertiary)', color: 'var(--text-primary)' }}
          >
            {t('reauth.later')}
          </button>
          {provider && (
            <button
              type="button"
              autoFocus
              onClick={() => startSignIn(provider.slug)}
              style={{ padding: '7px 14px', borderRadius: 7, fontSize: 13, fontWeight: 500, cursor: 'pointer', border: 'none', background: 'var(--accent)', color: 'var(--accent-text)' }}
            >
              {t('reauth.signIn', { provider: provider.name || provider.slug })}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
