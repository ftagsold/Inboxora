// Sender favicons (preference, validation, cache headers on every answer) and Todoist
// integration state. Neither touches the network: upstream lookups are not compared.
import { U } from '../seed-mail.mjs';

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`UPDATE users SET preferences = '{"senderFavicons": false}'::jsonb WHERE id = $1`, [U.alice]);
  await sql(`UPDATE users SET preferences = '{}'::jsonb WHERE id = $1`, [U.bob]);
  await sql(`DELETE FROM user_integrations WHERE user_id IN ($1, $2)`, [U.alice, U.bob]);
  await sql(`INSERT INTO user_integrations (user_id, provider, config) VALUES ($1, 'todoist', '{"token": "enc:v1:00:00:00"}'::jsonb)`, [U.bob]);
  return { keep: [U.alice, U.bob] };
}

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const fav = (domain, as) => ({ path: `/api/sender-favicons/${domain}`, ...(as ? { as } : {}), compareHeaders: ['cache-control', 'content-type', 'retry-after'] });

export default [{
  name: 'sender favicons and Todoist integration state',
  seed,
  sessions,
  steps: [
    fav('example.com'),
    fav('example.com', 'alice'),
    fav('not_a_domain', 'bob'),
    fav('1.2.3.4', 'bob'),
    fav('a%40b.com', 'bob'),
    fav('localhost', 'bob'),
    { path: '/api/sender-favicons/', as: 'bob', compareHeaders: ['cache-control'] },
    { path: '/api/todoist/status', as: 'alice' },
    { path: '/api/todoist/status', as: 'bob' },
    { path: '/api/todoist/projects', as: 'alice' },
    { path: '/api/todoist/labels', as: 'bob' },
    { method: 'POST', path: '/api/todoist/tasks', as: 'alice', body: { content: 'x' } },
    { method: 'POST', path: '/api/todoist/connect', as: 'alice', body: {} },
    { method: 'POST', path: '/api/todoist/connect', as: 'alice', body: { token: '   ' } },
    { method: 'DELETE', path: '/api/todoist/disconnect', as: 'bob' },
    { path: '/api/todoist/status', as: 'bob' },
  ],
}];
