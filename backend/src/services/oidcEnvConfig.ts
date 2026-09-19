// Parses OIDC_<N>_* environment variables (N = 1, 2, 3, ...) into provider configs, for
// docker/podman deployments that want to declare SSO providers — and optionally an IMAP/SMTP
// mailbox auto-provisioned via that provider's OIDC token — without going through the admin UI.
//
// Env vars are read once at import time (they don't change for the life of a container) and
// cached; call reloadOidcEnvProviders() in tests to re-parse after mutating process.env.
import { normalizeClaimName } from '../utils/claimName.js';

const MAX_PROVIDER_INDEX = 50;
const SMTP_TLS_VALUES = new Set(['STARTTLS', 'SSL', 'none']);

export interface OidcMailConfig {
  imapHost: string;
  imapPort: number;
  imapTls: boolean;
  smtpHost: string;
  smtpPort: number;
  smtpTls: string;
  usernameClaim: string;
  skipTlsVerify: boolean;
  displayName: string | null;
}

export interface OidcEnvProviderConfig {
  index: number;
  name: string;
  slug: string;
  issuerUrl: string;
  clientId: string;
  clientSecret: string;
  scopes: string;
  provisioningMode: string;
  allowedDomains: string | null;
  enabled: boolean;
  requireEmailVerified: boolean;
  allowInsecure: boolean;
  adminGroupClaim: string | null;
  adminGroupValue: string | null;
  rpInitiatedLogout: boolean;
  loginMatchClaim: string;
  mail: OidcMailConfig | null;
}

function envBool(value: string | undefined, fallback: boolean): boolean {
  if (value === undefined || value === null || value === '') return fallback;
  return value === 'true' || value === '1';
}

function envInt(value: string | undefined, fallback: number): number {
  const n = Number(value);
  return value !== undefined && value !== '' && Number.isInteger(n) ? n : fallback;
}

// Returns { config, errors } for provider index N, or null if no OIDC_<N>_* vars are set at
// all (an unused index — not an error, just absent). `config` is null when required fields are
// missing/invalid; `errors` explains why so seeding can log a clear message and skip it.
function parseProvider(n: number): { config: OidcEnvProviderConfig | null; errors: string[] } | null {
  const p = (key: string) => process.env[`OIDC_${n}_${key}`];
  const hasAny = ['ISSUER_URL', 'CLIENT_ID', 'CLIENT_SECRET', 'NAME', 'SLUG'].some(k => p(k) !== undefined);
  if (!hasAny) return null;

  const errors: string[] = [];
  const issuerUrl = p('ISSUER_URL')?.trim();
  const clientId = p('CLIENT_ID')?.trim();
  const clientSecret = p('CLIENT_SECRET');
  const allowInsecure = envBool(p('ALLOW_INSECURE'), false);

  if (!issuerUrl) errors.push(`OIDC_${n}_ISSUER_URL is required`);
  else {
    try {
      const parsed = new URL(issuerUrl);
      if (!allowInsecure && parsed.protocol !== 'https:') errors.push(`OIDC_${n}_ISSUER_URL must use HTTPS (or set OIDC_${n}_ALLOW_INSECURE=true)`);
    } catch {
      errors.push(`OIDC_${n}_ISSUER_URL is not a valid URL`);
    }
  }
  if (!clientId) errors.push(`OIDC_${n}_CLIENT_ID is required`);
  if (!clientSecret) errors.push(`OIDC_${n}_CLIENT_SECRET is required`);

  const slug = (p('SLUG')?.trim() || `sso-${n}`).toLowerCase();
  if (!/^[a-z0-9-]+$/.test(slug)) errors.push(`OIDC_${n}_SLUG must contain only lowercase letters, numbers and hyphens`);

  const provisioningMode = p('PROVISIONING_MODE')?.trim() || 'login_existing_only';
  if (!['disabled', 'login_existing_only', 'open'].includes(provisioningMode)) {
    errors.push(`OIDC_${n}_PROVISIONING_MODE must be one of: disabled, login_existing_only, open`);
  }

  const loginMatchClaimRaw = p('LOGIN_MATCH_CLAIM')?.trim();
  const loginMatchClaim = loginMatchClaimRaw ? normalizeClaimName(loginMatchClaimRaw) : 'email';
  if (loginMatchClaimRaw && !loginMatchClaim) errors.push(`OIDC_${n}_LOGIN_MATCH_CLAIM is not a valid claim name`);

  const adminGroupClaimRaw = p('ADMIN_GROUP_CLAIM')?.trim();
  const adminGroupClaim = adminGroupClaimRaw ? normalizeClaimName(adminGroupClaimRaw) : null;
  if (adminGroupClaimRaw && !adminGroupClaim) errors.push(`OIDC_${n}_ADMIN_GROUP_CLAIM is not a valid claim name`);

  // Mail-passthrough config is optional and additive: a mistake here disables auto-provisioning
  // for this provider (logged separately below) but must not also take down SSO login itself.
  let mail: OidcMailConfig | null = null;
  const mailErrors: string[] = [];
  const mailImapHost = p('MAIL_IMAP_HOST')?.trim();
  const mailSmtpHost = p('MAIL_SMTP_HOST')?.trim();
  if (mailImapHost || mailSmtpHost) {
    if (!mailImapHost || !mailSmtpHost) {
      mailErrors.push(`OIDC_${n}_MAIL_IMAP_HOST and OIDC_${n}_MAIL_SMTP_HOST must both be set to enable mailbox auto-provisioning for provider ${n}`);
    } else {
      const smtpTls = p('MAIL_SMTP_TLS')?.trim() || 'STARTTLS';
      if (!SMTP_TLS_VALUES.has(smtpTls)) mailErrors.push(`OIDC_${n}_MAIL_SMTP_TLS must be one of: ${[...SMTP_TLS_VALUES].join(', ')}`);
      const usernameClaimRaw = p('MAIL_USERNAME_CLAIM')?.trim();
      const usernameClaim = usernameClaimRaw ? normalizeClaimName(usernameClaimRaw) : 'email';
      if (usernameClaimRaw && !usernameClaim) mailErrors.push(`OIDC_${n}_MAIL_USERNAME_CLAIM is not a valid claim name`);
      if (!mailErrors.length) {
        mail = {
          imapHost: mailImapHost,
          imapPort: envInt(p('MAIL_IMAP_PORT'), 993),
          imapTls: envBool(p('MAIL_IMAP_TLS'), true),
          smtpHost: mailSmtpHost,
          smtpPort: envInt(p('MAIL_SMTP_PORT'), 587),
          smtpTls,
          usernameClaim: usernameClaim || 'email',
          skipTlsVerify: envBool(p('MAIL_SKIP_TLS_VERIFY'), false),
          displayName: p('MAIL_NAME')?.trim() || null,
        };
      }
    }
  }
  if (mailErrors.length) {
    console.error(`OIDC env provider OIDC_${n}_* mailbox auto-provisioning is misconfigured and will be disabled (SSO login is unaffected):\n  - ${mailErrors.join('\n  - ')}`);
  }

  if (errors.length) return { config: null, errors };

  return {
    config: {
      index: n,
      name: p('NAME')?.trim() || `SSO ${n}`,
      slug,
      issuerUrl: issuerUrl as string,
      clientId: clientId as string,
      clientSecret: clientSecret as string,
      scopes: p('SCOPES')?.trim() || 'openid email profile',
      provisioningMode,
      allowedDomains: p('ALLOWED_DOMAINS')?.trim() || null,
      enabled: envBool(p('ENABLED'), true),
      requireEmailVerified: envBool(p('REQUIRE_EMAIL_VERIFIED'), true),
      allowInsecure,
      adminGroupClaim,
      adminGroupValue: adminGroupClaim ? (p('ADMIN_GROUP_VALUE')?.trim() || null) : null,
      rpInitiatedLogout: envBool(p('RP_INITIATED_LOGOUT'), false),
      loginMatchClaim: loginMatchClaim || 'email',
      mail,
    },
    errors: [],
  };
}

function parseAll(): OidcEnvProviderConfig[] {
  const providers: OidcEnvProviderConfig[] = [];
  for (let n = 1; n <= MAX_PROVIDER_INDEX; n++) {
    const result = parseProvider(n);
    if (!result) continue;
    if (result.errors.length) {
      console.error(`OIDC env provider OIDC_${n}_* is misconfigured and will be skipped:\n  - ${result.errors.join('\n  - ')}`);
      continue;
    }
    if (result.config) providers.push(result.config);
  }
  const seenSlugs = new Map<string, number>();
  for (const config of providers) {
    if (seenSlugs.has(config.slug)) {
      console.error(`OIDC env providers OIDC_${seenSlugs.get(config.slug)}_* and OIDC_${config.index}_* both resolve to slug "${config.slug}" — set distinct OIDC_<N>_SLUG values`);
    }
    seenSlugs.set(config.slug, config.index);
  }
  return providers;
}

let cachedProviders: OidcEnvProviderConfig[] | null = null;

export function getOidcEnvProviders(): OidcEnvProviderConfig[] {
  if (!cachedProviders) cachedProviders = parseAll();
  return cachedProviders;
}

export function getMailConfigForSlug(slug: string): OidcMailConfig | null {
  return getOidcEnvProviders().find(p => p.slug === slug)?.mail || null;
}

// Test-only: force re-parsing process.env on the next call.
export function reloadOidcEnvProviders(): void {
  cachedProviders = null;
}
