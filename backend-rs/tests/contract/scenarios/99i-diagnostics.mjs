// POST /api/diagnostics/report: allowlisted, salted-hash report sections. Runtime sections
// (uptime, latency, connection counters, warning ring, versions) differ by process.
import { A, U, seedMail } from '../seed-mail.mjs';

async function seed(ctx) {
  const out = await seedMail(ctx);
  await ctx.sql(`UPDATE email_accounts SET sync_error = 'getaddrinfo ENOTFOUND imap.work.test', imap_host = 'imap.gmail.com' WHERE id = $1`, [A.aliceWork]);
  await ctx.sql(`UPDATE email_accounts SET oauth_provider = 'microsoft', last_sync = NULL WHERE id = $1`, [A.aliceHome]);
  await ctx.sql(`INSERT INTO folders (account_id, path, name, special_use, total_count, unread_count) VALUES ($1, 'Clients/X', 'Client X Invoices', NULL, 4, 1)`, [A.aliceWork]);
  await ctx.sql(`DELETE FROM system_settings WHERE key = 'ai_config'`);
  return out;
}

const sessions = () => ({ alice: { userId: U.alice } });
const report = (salt) => ({ method: 'POST', path: '/api/diagnostics/report', as: 'alice', body: { salt }, ignore: ['versions', 'server.uptimeSeconds', 'performance', 'connection', 'warnings', 'syncSignals', 'scrub'] });

export default [{
  name: 'diagnostics report: hashed refs, folder labels, counts and validation',
  seed,
  sessions,
  steps: [
    { method: 'POST', path: '/api/diagnostics/report', as: 'alice', body: { salt: 'xyz' } },
    { method: 'POST', path: '/api/diagnostics/report', as: 'alice', body: {} },
    report('0123456789abcdef'),
    report('fedcba98765432100123456789abcdef'),
  ],
}];
