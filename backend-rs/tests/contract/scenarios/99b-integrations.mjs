// /api/integrations: admin-managed Microsoft OAuth app credentials.
import { U } from '../seed-mail.mjs';

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`UPDATE users SET is_admin = true WHERE id = $1`, [U.alice]);
  await sql(`UPDATE users SET is_admin = false WHERE id = $1`, [U.bob]);
  await sql('DELETE FROM integration_config');
  return { keep: [U.alice, U.bob] };
}

const sessions = () => ({ admin: { userId: U.alice }, user: { userId: U.bob } });

export default [{
  name: 'integration configs: save, mask, keep secret, status, delete',
  seed,
  sessions,
  steps: [
    { path: '/api/integrations/status' },
    { path: '/api/integrations/status', as: 'user' },
    { path: '/api/integrations', as: 'user' },
    { path: '/api/integrations', as: 'admin' },
    { method: 'POST', path: '/api/integrations/google', as: 'admin', body: { clientId: 'x' } },
    { method: 'POST', path: '/api/integrations/microsoft', as: 'user', body: { clientId: 'x' } },
    { method: 'POST', path: '/api/integrations/microsoft', as: 'admin', body: { clientId: 'app-1', clientSecret: 'shh', tenantId: 'common', redirectUri: 'http://localhost:4100/oauth/microsoft/callback' } },
    { path: '/api/integrations', as: 'admin' },
    { path: '/api/integrations/status', as: 'user' },
    { method: 'POST', path: '/api/integrations/microsoft', as: 'admin', body: { clientId: 'app-2', clientSecret: '••••••••', tenantId: '' } },
    { path: '/api/integrations', as: 'admin' },
    { method: 'DELETE', path: '/api/integrations/microsoft', as: 'admin' },
    { path: '/api/integrations/status', as: 'user' },
    { path: '/api/integrations', as: 'admin' },
  ],
  check: {
    8: ({ result }) => { if (result.json?.microsoft?.configured !== true) throw new Error('saved config must apply to the environment'); },
    9: async ({ sql }) => {
      const { rows } = await sql(`SELECT config->>'clientSecret' AS s FROM integration_config WHERE provider = 'microsoft'`);
      if (!rows[0]?.s?.startsWith('enc:v1:')) throw new Error(`the kept secret must stay encrypted: ${rows[0]?.s}`);
    },
  },
}];
