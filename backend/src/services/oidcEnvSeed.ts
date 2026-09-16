// One-time bootstrap: create an oidc_providers row for each OIDC_<N>_* environment provider
// that doesn't already exist (matched by slug). This only ever creates rows — once a
// provider exists (whether seeded from env or created in the admin UI), the admin UI owns it
// and further changes belong there; restarting the container with different OIDC_<N>_* values
// does not modify an already-seeded provider. This keeps env-seeded and admin-managed
// providers interchangeable after the first boot, instead of fighting each other on every
// restart.
import { query } from './db.js';
import { encrypt } from './encryption.js';
import { validateHost } from './hostValidation.js';
import { getOidcEnvProviders } from './oidcEnvConfig.js';
import { toAppError } from '../utils/errors.js';

export async function seedOidcProvidersFromEnv(): Promise<void> {
  const providers = getOidcEnvProviders();
  for (const p of providers) {
    try {
      if (!p.allowInsecure) {
        const hostErr = await validateHost(new URL(p.issuerUrl).hostname);
        if (hostErr) {
          console.error(`OIDC env provider OIDC_${p.index}_* skipped: issuer host rejected: ${hostErr}`);
          continue;
        }
      }

      const existing = await query('SELECT id FROM oidc_providers WHERE slug = $1', [p.slug]);
      if (existing.rows.length) continue;

      await query(
        `INSERT INTO oidc_providers (name, slug, issuer_url, client_id, client_secret, scopes, provisioning_mode, allowed_domains, enabled, require_email_verified, allow_insecure, admin_group_claim, admin_group_value, rp_initiated_logout, login_match_claim)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)`,
        [
          p.name, p.slug, p.issuerUrl, p.clientId, encrypt(p.clientSecret),
          p.scopes, p.provisioningMode, p.allowedDomains, p.enabled, p.requireEmailVerified,
          p.allowInsecure, p.adminGroupClaim, p.adminGroupValue, p.rpInitiatedLogout, p.loginMatchClaim,
        ]
      );
      console.log(`Seeded OIDC provider "${p.name}" (slug: ${p.slug}) from OIDC_${p.index}_* env vars`);
    } catch (caught) {
      const err = toAppError(caught);
      console.error(`Failed to seed OIDC provider OIDC_${p.index}_* (slug: ${p.slug}):`, err.message);
    }
  }
}
