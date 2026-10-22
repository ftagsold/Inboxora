// /oauth/microsoft: the flows up to the Microsoft token endpoint (the authority is fixed in
// Node, so token exchange itself is not compared here).
import { U } from '../seed-mail.mjs';

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice]);
  await sql(`UPDATE users SET is_admin = true WHERE id = $1`, [U.alice]);
  await sql('DELETE FROM integration_config');
  return { keep: [U.alice] };
}

const HEADERS = ['content-type', 'vary'];
const random = /(?<=state=)[0-9a-f]{32}/g;

export default [{
  name: 'Microsoft OAuth start, callback validation and device poll',
  seed,
  sessions: () => ({ alice: { userId: U.alice } }),
  steps: [
    { path: '/oauth/microsoft' },
    { method: 'DELETE', path: '/api/integrations/microsoft', as: 'alice' },
    { path: '/oauth/microsoft', as: 'alice' },
    { method: 'POST', path: '/oauth/microsoft/device', as: 'alice', csrf: false, body: {} },
    { path: '/oauth/microsoft/device/poll', as: 'alice' },
    { path: '/oauth/microsoft/device/poll' },
    { method: 'POST', path: '/api/integrations/microsoft', as: 'alice', body: { clientId: 'app-1', clientSecret: 's', tenantId: 'contoso.example', redirectUri: 'http://localhost:4100/oauth/microsoft/callback' } },
    { path: '/oauth/microsoft', as: 'alice', compareHeaders: HEADERS, textPattern: random, capture: (_j, _c, raw) => ({ state: new URL(raw.location).searchParams.get('state') }) },
    { path: '/oauth/microsoft/callback?error=access_denied&error_description=No%20thanks', as: 'alice' },
    { path: '/oauth/microsoft/callback?state=wrong&code=x', as: 'alice' },
    { path: '/oauth/microsoft/callback?code=x', as: 'alice' },
    { method: 'DELETE', path: '/api/integrations/microsoft', as: 'alice' },
  ],
  check: {
    7: ({ result, ctx }) => {
      if (result.status !== 302 || !ctx.state) throw new Error(`start must redirect to Microsoft: ${result.status}`);
    },
  },
}];
