import { query } from './db.js';

/**
 * PASSWORD_LOGIN_DISABLED=true turns off username/password sign-in (login, registration,
 * password reset requests) regardless of the admin setting, so a deployment can enforce SSO
 * from its environment. The admin UI then shows the setting as locked.
 */
export function passwordLoginDisabledByEnv(env: NodeJS.ProcessEnv = process.env): boolean {
  return /^(1|true|yes|on)$/i.test((env.PASSWORD_LOGIN_DISABLED || '').trim());
}

/** Whether password sign-in is off, by environment or by the admin setting. */
export async function isPasswordLoginDisabled(): Promise<boolean> {
  if (passwordLoginDisabledByEnv()) return true;
  const setting = await query<{ value: string }>("SELECT value FROM system_settings WHERE key = 'internal_auth_disabled'");
  return setting.rows[0]?.value === 'true';
}
