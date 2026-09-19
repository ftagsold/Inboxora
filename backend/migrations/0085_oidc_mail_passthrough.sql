-- Links an auto-provisioned IMAP/SMTP mailbox (oauth_provider = 'oidc') back to the OIDC
-- provider whose access/refresh token authenticates it, so the token can be refreshed via
-- that provider's token endpoint without guessing which provider issued it.
ALTER TABLE email_accounts ADD COLUMN IF NOT EXISTS oidc_provider_id UUID REFERENCES oidc_providers(id) ON DELETE SET NULL;

-- One auto-provisioned mailbox per (user, provider): re-running provisioning on a later
-- login updates the existing row (fresh tokens, current env-configured host) instead of
-- creating a duplicate account every time the user logs in.
CREATE UNIQUE INDEX IF NOT EXISTS idx_email_accounts_user_oidc_provider
  ON email_accounts(user_id, oidc_provider_id)
  WHERE oidc_provider_id IS NOT NULL;
