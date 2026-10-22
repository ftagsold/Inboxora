// /api/search
import { seedMail, U, A } from '../seed-mail.mjs';

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const q = (s) => `/api/search?q=${encodeURIComponent(s)}`;

export default [
  {
    name: 'operators, terms, scope and trash exclusion',
    seed: seedMail, sessions,
    steps: [
      { path: '/api/search', as: 'alice' },
      { path: q('invoice'), as: 'alice' },
      { path: q('invoice in:trash'), as: 'alice' },
      { path: q('invoice in:all'), as: 'alice' },
      { path: q('in:inbox'), as: 'alice' },
      { path: q('in:2026 invoice'), as: 'alice' },
      { path: `${q('invoice')}&folder=Sent`, as: 'alice' },
      { path: q('from:billing -is:read'), as: 'alice' },
      { path: q('from:"Billing Team" has:attachment'), as: 'alice' },
      { path: q('subject:lunch is:starred'), as: 'alice' },
      { path: q('to:alice -invoice'), as: 'alice' },
      { path: q('invoice -quarterly'), as: 'alice' },
      { path: q('a b'), as: 'alice' },
      { path: `${q('invoice')}&accountId=${A.aliceHidden}`, as: 'alice' },
      { path: `${q('invoice')}&accountId=${A.bob}`, as: 'alice' },
      { path: `${q('invoice')}&limit=1&offset=1`, as: 'alice' },
      { path: `${q('invoice')}&limit=0`, as: 'alice' },
      { path: q('x'.repeat(501)), as: 'alice' },
      { path: q('invoice'), as: 'bob' },
    ],
  },
  {
    name: 'contact suggestions',
    seed: async (ctx) => {
      const r = await seedMail(ctx);
      await ctx.sql(`DELETE FROM contacts WHERE user_id = $1`, [U.alice]);
      await ctx.sql(`INSERT INTO address_books (user_id, name) VALUES ($1, 'Personal') ON CONFLICT DO NOTHING`, [U.alice]);
      await ctx.sql(`INSERT INTO contacts (address_book_id, user_id, uid, display_name, primary_email, is_auto, send_count) SELECT id, $1, 'k1', 'Billing Known', 'billing@acme.test', false, 5 FROM address_books WHERE user_id = $1 AND name = 'Personal'`, [U.alice]);
      await ctx.sql(`INSERT INTO contacts (address_book_id, user_id, uid, display_name, primary_email, is_auto, send_count) SELECT id, $1, 'k2', 'Carol Auto', 'carol@corp.test', true, 0 FROM address_books WHERE user_id = $1 AND name = 'Personal'`, [U.alice]);
      return r;
    },
    sessions,
    steps: [
      { path: '/api/search/contacts?q=b', as: 'alice' },
      { path: '/api/search/contacts?q=bi', as: 'alice' },
      { path: '/api/search/contacts?q=o', as: 'alice' },
      { path: '/api/search/contacts?q=co', as: 'alice' },
      { path: '/api/search/contacts?q=shop', as: 'alice' },
      { path: `/api/search/contacts?q=${'x'.repeat(101)}`, as: 'alice' },
    ],
  },
  {
    name: 'rate limit: 20 per minute shared across endpoints',
    seed: seedMail,
    sessions: () => ({ carol: { userId: U.bob } }),
    steps: [
      ...Array.from({ length: 19 }, () => ({ path: q('invoice'), as: 'carol' })),
      { path: '/api/search/contacts?q=bi', as: 'carol' },
      { path: q('invoice'), as: 'carol', compareHeaders: ['retry-after'] },
    ],
  },
];
