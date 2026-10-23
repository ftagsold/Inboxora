-- Set when a mailbox's OAuth/OIDC sign-in is no longer accepted (refresh token rejected or
-- the IMAP server refuses a freshly refreshed token). Cleared by a successful connect or a
-- new sign-in; the web app asks the user to sign in again while it is set.
ALTER TABLE email_accounts
  ADD COLUMN IF NOT EXISTS reauth_required BOOLEAN NOT NULL DEFAULT false;
