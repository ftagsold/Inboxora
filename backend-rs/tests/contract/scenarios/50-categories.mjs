// /api/categories/*
import { seedMail, U, A, M } from '../seed-mail.mjs';

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });

export default [{
  name: 'sources, recategorize, ai-classify',
  seed: async (ctx) => {
    const r = await seedMail(ctx);
    await ctx.sql('DELETE FROM category_list_sources WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
    await ctx.sql('UPDATE email_accounts SET categorization_enabled = true WHERE id = $1', [A.aliceWork]);
    await ctx.sql('UPDATE messages SET is_bulk = true WHERE id = $1', [M(1)]);
    return r;
  },
  sessions,
  steps: [
    { path: '/api/categories/sources' },
    { path: '/api/categories/unknown' },
    { path: '/api/categories/sources', as: 'alice' },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'manual', value: ' Bob@X.test ', label: ' Bob ' }, capture: (j) => ({ s1: j.source.id }) },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'manual', value: 'bob@x.test' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'builtin', value: 'social_networks' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'builtin', value: 'nope' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'manual', value: 'not a domain' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'url', value: 'http://lists.test/x.txt' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'url', value: 'https://localhost/x.txt' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'url', value: 'https://10.0.0.1/x.txt' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'url', value: 'not a url' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'rss', value: 'x' } },
    { method: 'POST', path: '/api/categories/sources', as: 'alice', body: { sourceType: 'manual' } },
    { path: '/api/categories/sources', as: 'alice' },
    (c) => ({ method: 'PATCH', path: `/api/categories/sources/${c.s1}`, as: 'alice', body: { enabled: false } }),
    (c) => ({ method: 'PATCH', path: `/api/categories/sources/${c.s1}`, as: 'alice', body: { enabled: 'no' } }),
    (c) => ({ method: 'PATCH', path: `/api/categories/sources/${c.s1}`, as: 'bob', body: { enabled: true } }),
    (c) => ({ method: 'POST', path: `/api/categories/sources/${c.s1}/refresh`, as: 'alice' }),
    { method: 'POST', path: '/api/categories/sources/00000000-0000-4000-8000-000000000999/refresh', as: 'alice' },
    (c) => ({ method: 'PATCH', path: `/api/categories/sources/${c.s1}`, as: 'alice', body: { enabled: true } }),
    { method: 'POST', path: `/api/categories/recategorize/${A.aliceHome}`, as: 'alice' },
    { method: 'POST', path: `/api/categories/recategorize/${A.aliceWork}`, as: 'alice' },
    { method: 'POST', path: '/api/categories/ai-classify/nope', as: 'alice' },
    { method: 'POST', path: `/api/categories/ai-classify/${M(7)}`, as: 'alice' },
    { method: 'POST', path: `/api/categories/ai-classify/${M(1)}`, as: 'alice' },
    (c) => ({ method: 'DELETE', path: `/api/categories/sources/${c.s1}`, as: 'alice' }),
    (c) => ({ method: 'DELETE', path: `/api/categories/sources/${c.s1}`, as: 'alice' }),
  ],
  after: async ({ sql }) => {
    await new Promise(r => setTimeout(r, 300));
    return {
      sources: (await sql('SELECT source_type, value, label, enabled FROM category_list_sources ORDER BY value')).rows,
      categories: (await sql('SELECT id, category FROM messages WHERE account_id = $1 ORDER BY id', [A.aliceWork])).rows,
    };
  },
}];
