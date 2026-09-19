// Auto-provisions and refreshes the IMAP/SMTP mailbox for OIDC "token passthrough" —
// deployments where the same OIDC provider issues both the web login session and the
// credential the mail server accepts, so no separate mailbox password is ever collected.
// See docs/wiki/Configuration.md for the env vars (OIDC_<N>_MAIL_*) that opt a provider in.
import { query } from './db.js';
import { encrypt, decrypt } from './encryption.js';
import { getDiscovery, makeInsecureFetch } from './oidcDiscovery.js';
import { resolveClaimValue } from '../utils/claimName.js';
import { getMailConfigForSlug } from './oidcEnvConfig.js';
import type { EmailAccountRow } from './imapManager.js';

const ACCOUNT_COLOR = '#0ea5e9';

/** The OIDC provider fields this module reads (a subset of oidc_providers). */
interface OidcProviderForMail {
  id: string;
  name: string;
  slug: string;
}

/** The token endpoint response shape this module reads. */
interface OidcTokenPayload {
  access_token?: string;
  refresh_token?: string;
  expires_in?: number;
  id_token?: string;
  error?: string;
  error_description?: string;
}

function tokenExpiry(expiresIn: number | null | undefined): Date {
  const secs = Number.isFinite(expiresIn) && (expiresIn as number) > 0 ? (expiresIn as number) : 3600;
  return new Date(Date.now() + secs * 1000);
}

// Called right after a successful OIDC login (existing identity, login_existing_only match,
// or open provisioning) — before imapManager.connectAllForUser so the new/updated account is
// picked up by that same connect pass. Never throws: callers treat this as best-effort and
// must not fail the login over a mail-provisioning problem.
export async function provisionOidcMailAccount({ userId, provider, payload, email, tokenData }: {
  userId: string;
  provider: OidcProviderForMail;
  payload: Record<string, unknown> | null | undefined;
  email: string | null | undefined;
  tokenData: OidcTokenPayload;
}): Promise<void> {
  const mail = getMailConfigForSlug(provider.slug);
  if (!mail) return;
  if (!tokenData?.access_token) return;

  const username = resolveClaimValue(mail.usernameClaim, payload, email);
  if (!username) {
    console.warn(`OIDC mail auto-provision: claim "${mail.usernameClaim}" is missing from the id_token for user ${userId} — skipping mailbox provisioning`);
    return;
  }
  // email_address is NOT NULL — the compose/display address always comes from the verified
  // 'email' claim, independent of which claim names the IMAP/SMTP login (auth_user).
  if (!email) {
    console.warn(`OIDC mail auto-provision: no verified email claim for user ${userId} — skipping mailbox provisioning`);
    return;
  }

  await query(
    `INSERT INTO email_accounts (
       user_id, name, email_address, color, protocol,
       imap_host, imap_port, imap_tls, imap_skip_tls_verify,
       smtp_host, smtp_port, smtp_tls,
       auth_user, oauth_provider, oauth_access_token, oauth_refresh_token, oauth_token_expiry,
       oidc_provider_id
     ) VALUES ($1,$2,$3,$4,'imap',$5,$6,$7,$8,$9,$10,$11,$12,'oidc',$13,$14,$15,$16)
     ON CONFLICT (user_id, oidc_provider_id) WHERE oidc_provider_id IS NOT NULL DO UPDATE SET
       auth_user = excluded.auth_user,
       email_address = excluded.email_address,
       imap_host = excluded.imap_host, imap_port = excluded.imap_port, imap_tls = excluded.imap_tls,
       imap_skip_tls_verify = excluded.imap_skip_tls_verify,
       smtp_host = excluded.smtp_host, smtp_port = excluded.smtp_port, smtp_tls = excluded.smtp_tls,
       oauth_access_token = excluded.oauth_access_token,
       oauth_refresh_token = COALESCE(excluded.oauth_refresh_token, email_accounts.oauth_refresh_token),
       oauth_token_expiry = excluded.oauth_token_expiry,
       sync_error = NULL`,
    [
      userId, mail.displayName || provider.name, email, ACCOUNT_COLOR,
      mail.imapHost, mail.imapPort, mail.imapTls, mail.skipTlsVerify,
      mail.smtpHost, mail.smtpPort, mail.smtpTls,
      username,
      encrypt(tokenData.access_token),
      tokenData.refresh_token ? encrypt(tokenData.refresh_token) : null,
      tokenExpiry(tokenData.expires_in),
      provider.id,
    ]
  );
}

/** The account columns the OIDC mailbox refresh path reads. */
type OidcRefreshAccount = Pick<EmailAccountRow, 'id'> & {
  oidc_provider_id?: string | null;
  oauth_refresh_token?: string | null;
};

/** An account row plus the plaintext token a refresh hands back to its caller.
 *  Partial because callers/tests may pass a subset of the row. */
type RefreshedOidcAccount = Partial<EmailAccountRow> & {
  oidc_provider_id?: string | null;
  oauth_refresh_token?: string | null;
  [key: string]: unknown;
};

interface OidcProviderRowForRefresh {
  id: string;
  issuer_url: string;
  client_id: string;
  client_secret: string;
  allow_insecure: boolean | null;
}

// Refreshes an 'oidc' account's access token via its provider's token endpoint. Mirrors
// refreshMicrosoftToken (routes/oauth.js) but always sends a client_secret — unlike Microsoft,
// oidc_providers.client_secret is NOT NULL, there is no public-client flow here.
export async function refreshOidcAccountToken(account: EmailAccountRow): Promise<EmailAccountRow>;
export async function refreshOidcAccountToken(account: OidcRefreshAccount): Promise<RefreshedOidcAccount>;
export async function refreshOidcAccountToken(account: OidcRefreshAccount): Promise<EmailAccountRow | RefreshedOidcAccount> {
  if (!account.oidc_provider_id) throw new Error('This mailbox is not linked to an OIDC provider — please log in again to reconnect it');

  const { rows } = await query<OidcProviderRowForRefresh>(
    'SELECT id, issuer_url, client_id, client_secret, allow_insecure FROM oidc_providers WHERE id = $1',
    [account.oidc_provider_id]
  );
  const provider = rows[0];
  if (!provider) throw new Error('The OIDC provider for this mailbox no longer exists');

  const refreshToken = decrypt(account.oauth_refresh_token);
  if (!refreshToken) throw new Error('OIDC refresh token is missing or corrupted — please log in again to reconnect this mailbox');
  const clientSecret = decrypt(provider.client_secret);
  if (!clientSecret) throw new Error('OIDC client secret decryption failed');

  const { doc } = await getDiscovery(provider.issuer_url, provider.allow_insecure);
  const params = new URLSearchParams({
    grant_type: 'refresh_token',
    refresh_token: refreshToken,
    client_id: provider.client_id,
    client_secret: clientSecret,
  });
  const tokenFetch = provider.allow_insecure ? makeInsecureFetch() : fetch;
  const tokenEndpoint = doc.token_endpoint;
  if (!tokenEndpoint) throw new Error('OIDC discovery missing required field: token_endpoint');
  const res = await tokenFetch(tokenEndpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded', Accept: 'application/json' },
    body: params.toString(),
    signal: AbortSignal.timeout(10000),
  });
  const tokens = (await res.json()) as OidcTokenPayload;
  if (!res.ok) throw new Error(tokens.error_description || tokens.error || 'OIDC token refresh failed');

  const expiry = tokenExpiry(tokens.expires_in);
  await query(
    `UPDATE email_accounts SET
       oauth_access_token = $1,
       oauth_refresh_token = COALESCE($2, oauth_refresh_token),
       oauth_token_expiry = $3
     WHERE id = $4`,
    [tokens.access_token ? encrypt(tokens.access_token) : null, tokens.refresh_token ? encrypt(tokens.refresh_token) : null, expiry, account.id]
  );

  // Return plaintext token so callers can use it immediately — decrypt() is a no-op on plaintext.
  return { ...account, oauth_access_token: tokens.access_token, oauth_token_expiry: expiry };
}
