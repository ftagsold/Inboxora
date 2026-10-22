// Shared mail fixtures: two users, accounts, folders and messages with fixed ids.
export const U = {
  alice: '00000000-0000-4000-8000-0000000000a1',
  bob: '00000000-0000-4000-8000-0000000000b0',
};
export const A = {
  aliceWork: '00000000-0000-4000-8000-00000000ac01',
  aliceHome: '00000000-0000-4000-8000-00000000ac02',
  aliceHidden: '00000000-0000-4000-8000-00000000ac03',
  bob: '00000000-0000-4000-8000-00000000ac04',
};
export const M = (n) => `00000000-0000-4000-8000-0000000${String(n).padStart(5, '0')}`;

export async function seedMail({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  // Invitation senders are ON DELETE RESTRICT; events left by an earlier scenario must go first.
  await sql(`DELETE FROM calendar_events WHERE invite_account_id IN (SELECT id FROM email_accounts WHERE user_id IN ($1, $2))`, [U.alice, U.bob]);
  await sql(`DELETE FROM email_accounts WHERE user_id IN ($1, $2)`, [U.alice, U.bob]);
  await sql(`INSERT INTO email_accounts (id, user_id, name, email_address, imap_host, enabled, protocol, include_in_unified_inbox, color) VALUES
    ($1, $5, 'Work', 'alice@work.test', 'imap.work.test', true, 'imap', true, '#f00'),
    ($2, $5, 'Home', 'alice@home.test', 'imap.home.test', true, 'imap', true, '#0f0'),
    ($3, $5, 'Hidden', 'alice@hidden.test', 'imap.hidden.test', true, 'imap', false, '#00f'),
    ($4, $6, 'Bob', 'bob@x.test', 'imap.x.test', true, 'imap', true, '#999')`, [A.aliceWork, A.aliceHome, A.aliceHidden, A.bob, U.alice, U.bob]);
  for (const acc of [A.aliceWork, A.aliceHome, A.aliceHidden, A.bob]) {
    await sql(`INSERT INTO folders (account_id, path, name, special_use, total_count, unread_count) VALUES ($1, 'INBOX', 'INBOX', NULL, 0, 0), ($1, 'Sent', 'Sent', '\\Sent', 0, 0), ($1, 'Trash', 'Trash', '\\Trash', 0, 0), ($1, 'Archive/2026', '2026', NULL, 0, 0)`, [acc]);
  }
  const msgs = [
    [1, A.aliceWork, 'INBOX', 'Quarterly invoice for ACME', 'Billing Team', 'billing@acme.test', '2026-10-01T10:00:00Z', false, false, true, 'Please find the invoice attached'],
    [2, A.aliceWork, 'INBOX', 'Re: Lunch tomorrow?', 'Bob Builder', 'bob@x.test', '2026-10-02T12:00:00Z', true, true, false, 'Sure, see you at noon'],
    [3, A.aliceHome, 'INBOX', 'Your package has shipped', 'Shop', 'noreply@shop.test', '2026-10-03T08:00:00Z', false, false, false, 'Tracking number 12345'],
    [4, A.aliceHome, 'Trash', 'Old invoice reminder', 'Billing Team', 'billing@acme.test', '2026-09-01T10:00:00Z', true, false, false, 'Reminder about the invoice'],
    [5, A.aliceWork, 'Sent', 'Invoice question', 'Alice', 'alice@work.test', '2026-10-04T09:00:00Z', true, false, false, 'Question about the invoice'],
    [6, A.aliceHidden, 'INBOX', 'Hidden account invoice', 'Billing Team', 'billing@acme.test', '2026-10-05T10:00:00Z', false, false, false, 'Invoice in hidden account'],
    [7, A.bob, 'INBOX', 'Bob private invoice', 'Billing Team', 'billing@acme.test', '2026-10-06T10:00:00Z', false, false, false, 'Not for alice'],
    [8, A.aliceWork, 'Archive/2026', 'Archived invoice', 'Carol', 'carol@corp.test', '2026-08-01T10:00:00Z', true, true, true, 'Archived invoice body'],
  ];
  for (const [n, acc, folder, subject, fromName, fromEmail, date, isRead, starred, att, body] of msgs) {
    await sql(`INSERT INTO messages (id, account_id, uid, folder, subject, from_name, from_email, date, is_read, is_starred, has_attachments, body_text, snippet, to_addresses, message_id)
      VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12, $13::jsonb, $14)`,
      [M(n), acc, n, folder, subject, fromName, fromEmail, date, isRead, starred, att, body, JSON.stringify([{ name: 'Alice', email: 'alice@work.test' }]), `<m${n}@fixture.test>`]);
  }
  return { keep: [U.alice, U.bob, ...Object.values(A), ...msgs.map(([n]) => M(n))] };
}
