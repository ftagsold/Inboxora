// /api/mail read paths and /api/accounts management, against the shared fixtures. IMAP hosts
// in the fixtures do not resolve, so engine-backed paths exercise their failure handling.
import { A, M, U, seedMail } from '../seed-mail.mjs';

async function seed(ctx) {
  const out = await seedMail(ctx);
  await ctx.sql(`UPDATE messages SET category = 'newsletter', is_bulk = true, list_unsubscribe = '<mailto:unsub@shop.test>' WHERE id = $1`, [M(3)]);
  await ctx.sql(`UPDATE messages SET body_html = '<p>Hello <img src="https://img.test/x.png"></p>', sender_email = 'agent@acme.test' WHERE id = $1`, [M(1)]);
  await ctx.sql(`UPDATE folders SET total_count = 2, unread_count = 1 WHERE account_id = $1 AND path = 'INBOX'`, [A.aliceWork]);
  await ctx.sql(`DELETE FROM account_aliases WHERE account_id = $1`, [A.aliceWork]);
  return out;
}

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const g = (path, extra = {}) => ({ path, as: 'alice', ...extra });

export default [{
  name: 'mail reads, categories, cleanup and accounts',
  seed,
  sessions,
  // gtd_* are added by Node's GTD plugin, which the Rust backend does not port.
  ignoreKeys: ['created_at', 'last_sync', 'sync_error', 'updated_at', 'gtd_enabled', 'gtd_folders'],
  steps: [
    g('/api/mail/messages'),
    g(`/api/mail/messages?accountId=${A.aliceWork}`),
    g(`/api/mail/messages?accountId=${A.aliceWork}&folder=Archive%2F2026`),
    g('/api/mail/messages?unreadOnly=true'),
    g('/api/mail/messages?threaded=true'),
    g(`/api/mail/messages?accountId=${A.aliceWork}&threaded=true&limit=1`),
    g('/api/mail/messages?category=newsletter'),
    g('/api/mail/messages?category=primary'),
    g('/api/mail/messages?folder=%01bad'),
    g(`/api/mail/messages?accountId=${A.bob}`),
    g(`/api/mail/messages/${M(1)}`),
    g(`/api/mail/messages/${M(7)}`),
    g('/api/mail/messages/not-a-uuid'),
    g('/api/mail/resolve-message?ref=%3Cm2%40fixture.test%3E'),
    g(`/api/mail/resolve-message?ref=${M(3)}`),
    g('/api/mail/resolve-message'),
    g('/api/mail/resolve-message?ref=x&accountId=nope'),
    g('/api/mail/unread-counts'),
    g('/api/mail/category-counts'),
    g(`/api/mail/category-counts?accountId=${A.aliceHome}`),
    g('/api/mail/category-counts?accountId=bad'),
    { method: 'PATCH', path: `/api/mail/messages/${M(2)}/category`, as: 'alice', body: { category: 'promotion' } },
    { method: 'PATCH', path: `/api/mail/messages/${M(2)}/category`, as: 'alice', body: { category: 'spam' } },
    { method: 'PATCH', path: `/api/mail/messages/${M(7)}/category`, as: 'alice', body: { category: 'primary' } },
    g(`/api/mail/mailbox-usage?accountId=${A.aliceHome}`),
    g('/api/mail/mailbox-usage'),
    g(`/api/mail/cleanup-preview?accountId=${A.aliceHome}&fromEmail=%20NOREPLY%40shop.test%20`),
    g(`/api/mail/cleanup-preview?accountId=${A.aliceHome}`),
    g(`/api/mail/messages/${M(1)}/body`),
    g(`/api/mail/messages/${M(1)}/body?remoteImages=1`),
    g(`/api/mail/messages/${M(2)}/headers`, { ignore: ['headers'] }),
    g(`/api/mail/messages/${M(1)}/attachments/2`),
    g(`/api/mail/messages/${M(2)}/attachments.zip`),
    { method: 'POST', path: `/api/mail/messages/${M(3)}/unsubscribe`, as: 'alice' },
    { method: 'POST', path: `/api/mail/messages/${M(2)}/unsubscribe`, as: 'alice' },
    { method: 'POST', path: '/api/mail/messages/bulk-read', as: 'alice', body: { ids: [M(1), M(7)], read: true } },
    { method: 'POST', path: '/api/mail/messages/bulk-read', as: 'alice', body: { ids: ['x'], read: true } },
    { method: 'POST', path: '/api/mail/messages/bulk-read', as: 'alice', body: { ids: [M(1)], read: 'yes' } },
    { method: 'POST', path: '/api/mail/sync', as: 'alice', body: { accountId: 'bad' } },
    { method: 'POST', path: '/api/mail/sync-folder', as: 'alice', body: { accountId: A.aliceWork } },
    { method: 'POST', path: '/api/mail/folders', as: 'alice', body: { accountId: A.aliceWork, name: '  ' } },
    { method: 'POST', path: '/api/mail/messages/bulk-move', as: 'alice', body: { ids: [M(1)], folder: '' } },
    { method: 'POST', path: `/api/mail/messages/${M(1)}/snooze`, as: 'alice', body: { until: '2000-01-01T00:00:00Z' } },
    { method: 'POST', path: `/api/mail/messages/${M(1)}/snooze`, as: 'alice', body: { until: 'soon' } },
    { method: 'POST', path: `/api/mail/messages/${M(1)}/ham`, as: 'alice' },
    g('/api/accounts'),
    g(`/api/accounts/${A.aliceWork}/folders`),
    g('/api/accounts/bad/folders'),
    { method: 'PUT', path: `/api/accounts/${A.aliceWork}`, as: 'alice', body: { color: '#123456', signature: '<p onclick="x()">Hi</p>', sort_order: 3 } },
    { method: 'PUT', path: `/api/accounts/${A.aliceWork}`, as: 'alice', body: { name: 'a\nb' } },
    { method: 'PUT', path: `/api/accounts/${A.aliceWork}`, as: 'alice', body: { unknown: 1 } },
    { method: 'PUT', path: `/api/accounts/${A.bob}`, as: 'alice', body: { color: '#000' } },
    { method: 'POST', path: `/api/accounts/${A.aliceWork}/aliases`, as: 'alice', body: { name: 'Sales', email: 'sales@work.test', reply_to: 'r@work.test' }, capture: (j) => ({ alias: j.id }) },
    { method: 'POST', path: `/api/accounts/${A.aliceWork}/aliases`, as: 'alice', body: { name: 'X' } },
    g(`/api/accounts/${A.aliceWork}/aliases`),
    (c) => ({ method: 'PUT', path: `/api/accounts/${A.aliceWork}/aliases/${c.alias}`, as: 'alice', body: { name: 'Sales 2', email: 'sales@work.test' } }),
    (c) => ({ method: 'DELETE', path: `/api/accounts/${A.aliceWork}/aliases/${c.alias}`, as: 'alice' }),
    (c) => ({ method: 'DELETE', path: `/api/accounts/${A.aliceWork}/aliases/${c.alias}`, as: 'alice' }),
    { method: 'POST', path: '/api/accounts', as: 'alice', body: { name: 'New' } },
    { method: 'POST', path: '/api/accounts', as: 'alice', body: { name: 'New', email_address: 'n@x.test', imap_host: '10.0.0.1' } },
    { method: 'POST', path: '/api/accounts', as: 'alice', body: { name: 'New', email_address: 'n@x.test', imap_port: 25, imap_host: 'imap.x.test', imap_tls: 'yes' } },
  ],
  after: async ({ sql }) => ({
    categories: (await sql(`SELECT id, category FROM messages WHERE id IN ($1, $2) ORDER BY id`, [M(2), M(7)])).rows,
    read: (await sql(`SELECT id, is_read FROM messages WHERE id IN ($1, $2) ORDER BY id`, [M(1), M(7)])).rows,
    account: (await sql(`SELECT color, signature, sort_order FROM email_accounts WHERE id = $1`, [A.aliceWork])).rows,
  }),
}];
