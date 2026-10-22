// /api/rules (CRUD and reorder; /run belongs to the mail engine)
import { seedMail, U, A } from '../seed-mail.mjs';

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const rule = (extra = {}) => ({ name: 'Invoices', conditions: [{ field: 'subject', operator: 'contains', value: 'invoice' }], actions: [{ type: 'markRead' }], ...extra });

export default [{
  name: 'create, validate, update, reorder, delete',
  seed: async (ctx) => { const r = await seedMail(ctx); await ctx.sql('DELETE FROM inbox_rules WHERE user_id IN ($1, $2)', [U.alice, U.bob]); return r; },
  sessions,
  steps: [
    { path: '/api/rules', as: 'alice' },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule(), capture: (j) => ({ r1: j.id }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ name: '', accountId: A.aliceWork, conditionLogic: 'OR', enabled: false, stopProcessing: 1, actions: [{ type: 'move', value: ' Archive/2026 ' }, { type: 'delete' }, { type: 'forward', value: ' fwd@x.test ' }, { type: 'forward', value: 'second@x.test' }, null, { type: 7 }] }), capture: (j) => ({ r2: j.id }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ actions: [{ type: 'move', value: 'Somewhere' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ accountId: A.aliceWork, actions: [{ type: 'move', value: 'Nope' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ accountId: A.bob }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: { conditions: 'x', actions: [] } },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ conditions: [{ field: 'subject', value: '  ' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ conditions: [{ field: 'header', value: 'x' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ conditions: [{ field: 'read_status', value: 'maybe' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ conditions: [{ field: 'subject', operator: 'regex', value: '(a+)+' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ conditions: [{ field: 'subject', operator: 'regex', value: '^inv\\d+' }] }), capture: (j) => ({ r3: j.id }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ conditions: [{ value: 'x' }] }) },
    { method: 'POST', path: '/api/rules', as: 'alice', body: rule({ actions: [{ type: 'forward', value: 'a@b' }] }) },
    { path: '/api/rules', as: 'alice' },
    (c) => ({ method: 'PUT', path: `/api/rules/${c.r1}`, as: 'alice', body: rule({ name: 'Renamed', accountId: A.aliceHome, actions: [{ type: 'archive' }] }) }),
    (c) => ({ method: 'PUT', path: `/api/rules/${c.r1}`, as: 'bob', body: rule() }),
    (c) => ({ method: 'PATCH', path: '/api/rules/reorder', as: 'alice', body: { ids: [c.r3, c.r1, c.r2] } }),
    (c) => ({ method: 'PATCH', path: '/api/rules/reorder', as: 'bob', body: { ids: [c.r3] } }),
    { method: 'PATCH', path: '/api/rules/reorder', as: 'alice', body: { ids: 'x' } },
    { method: 'PATCH', path: '/api/rules/reorder', as: 'alice', body: { ids: ['not-a-uuid'] } },
    { path: '/api/rules', as: 'alice' },
    (c) => ({ method: 'DELETE', path: `/api/rules/${c.r2}`, as: 'alice' }),
    (c) => ({ method: 'DELETE', path: `/api/rules/${c.r2}`, as: 'alice' }),
  ],
  after: async ({ sql }) => (await sql('SELECT name, account_id, enabled, stop_processing, priority, condition_logic, conditions, actions FROM inbox_rules ORDER BY priority, name')).rows,
}];
