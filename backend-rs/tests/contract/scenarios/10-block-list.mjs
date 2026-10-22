// /api/block-list
const ALICE = '00000000-0000-4000-8000-0000000000a1';
const BOB = '00000000-0000-4000-8000-0000000000b0';

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [ALICE, BOB]);
  await sql(`DELETE FROM block_list WHERE user_id IN ($1, $2)`, [ALICE, BOB]);
  await sql(`INSERT INTO block_list (user_id, email_address) VALUES ($1, 'bob-blocked@x.test')`, [BOB]);
  return { keep: [ALICE, BOB] };
}

export default [
  {
    name: 'requires authentication, including unknown subpaths',
    steps: [{ path: '/api/block-list' }, { path: '/api/block-list/whatever/deep' }, { method: 'POST', path: '/api/block-list', body: { emailAddress: 'x@y.test' } }],
  },
  {
    name: 'add, list, duplicate, remove, ownership',
    seed,
    sessions: () => ({ alice: { userId: ALICE }, ghost: { userId: '00000000-0000-4000-8000-00000000dead' } }),
    steps: [
      { path: '/api/block-list', as: 'alice' },
      { method: 'POST', path: '/api/block-list', as: 'alice', body: { emailAddress: '  Spam@Example.TEST ' }, capture: (j) => ({ entry: j.id }) },
      { method: 'POST', path: '/api/block-list', as: 'alice', body: { emailAddress: 'spam@example.test' } },
      { method: 'POST', path: '/api/block-list', as: 'alice', body: { emailAddress: '   ' } },
      { method: 'POST', path: '/api/block-list', as: 'alice', body: { emailAddress: 42 } },
      { method: 'POST', path: '/api/block-list', as: 'alice', body: 'emailAddress=a@b.test', headers: { 'content-type': 'application/x-www-form-urlencoded' } },
      { path: '/api/block-list', as: 'alice' },
      { path: '/api/block-list/unknown', as: 'alice' },
      (ctx) => ({ method: 'DELETE', path: `/api/block-list/${ctx.entry}`, as: 'alice' }),
      (ctx) => ({ method: 'DELETE', path: `/api/block-list/${ctx.entry}`, as: 'alice' }),
      { method: 'DELETE', path: '/api/block-list/not-a-uuid', as: 'alice' },
      { path: '/api/block-list', as: 'ghost' },
      { path: '/api/block-list', as: 'ghost' },
    ],
    after: async ({ sql }) => (await sql(`SELECT user_id, email_address FROM block_list ORDER BY email_address`)).rows,
  },
];
