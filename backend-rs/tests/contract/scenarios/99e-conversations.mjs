// Conversation Engine v2 endpoints under /api/mail against seeded conversations. Provider-backed
// actions (archive/move/delete) only exercise their validation and pre-IMAP failures.
import { A, M, U, seedMail } from '../seed-mail.mjs';

const C = (n) => `00000000-0000-4000-8000-00000000c0${String(n).padStart(2, '0')}`;
const L = (n) => `00000000-0000-4000-8000-00000000d0${String(n).padStart(2, '0')}`;

async function seed(ctx) {
  const out = await seedMail(ctx);
  const { sql } = ctx;
  const convs = [
    [1, U.alice, A.aliceWork, 'Quarterly invoice for ACME'],
    [2, U.alice, A.aliceWork, 'Archived invoice'],
    [3, U.alice, A.aliceHome, 'Your package has shipped'],
    [4, U.bob, A.bob, 'Bob private invoice'],
  ];
  for (const [n, user, acc, subject] of convs) {
    await sql(`INSERT INTO conversations (id, user_id, account_id, subject_snapshot, canonical_subject, created_at) VALUES ($1, $2, $3, $4, lower($4), '2026-07-01T00:00:00Z')`, [C(n), user, acc, subject]);
  }
  // [logical, message, conversation, user, account, parent, direction]
  const lms = [
    [1, 1, 1, U.alice, A.aliceWork, null, 'incoming'],
    [2, 2, 1, U.alice, A.aliceWork, 1, 'incoming'],
    [5, 5, 1, U.alice, A.aliceWork, 2, 'outgoing'],
    [8, 8, 2, U.alice, A.aliceWork, null, 'incoming'],
    [3, 3, 3, U.alice, A.aliceHome, null, 'incoming'],
    [7, 7, 4, U.bob, A.bob, null, 'incoming'],
  ];
  for (const [l, m, c, user, acc, parent, dir] of lms) {
    await sql(`INSERT INTO logical_messages (id, user_id, account_id, conversation_id, canonical_message_id, parent_logical_message_id, subject, direction, message_date, threading_reason, threading_confidence)
      SELECT $1, $2, $3, $4, 'm' || $6 || '@fixture.test', $5, subject, $7, date, 'references', 0.9 FROM messages WHERE id = $8`,
    [L(l), user, acc, C(c), parent ? L(parent) : null, String(m), dir, M(m)]);
    await sql(`UPDATE messages SET conversation_id = $1, conversation_user_id = $2, logical_message_id = $3, canonical_message_id = 'm' || $5 || '@fixture.test' WHERE id = $4`, [C(c), user, L(l), M(m), String(m)]);
  }
  await sql(`UPDATE messages SET body_html = '<p>Hi <img src="https://img.test/x.png"></p>' WHERE id = $1`, [M(1)]);
  return { keep: [...out.keep, ...[1, 2, 3, 4].map(C), ...[1, 2, 3, 5, 7, 8].map(L)] };
}

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const g = (path, extra = {}) => ({ path, as: 'alice', ...extra });
const p = (path, body, extra = {}) => ({ method: 'POST', path, as: 'alice', body, ...extra });

export default [{
  name: 'conversations: lists, detail, actions, overrides and rebuild',
  seed,
  sessions,
  ignoreKeys: ['created_at', 'updated_at', 'last_sync', 'sync_error'],
  steps: [
    g('/api/mail/conversations'),
    g(`/api/mail/conversations?accountId=${A.aliceWork}`),
    g(`/api/mail/conversations?accountId=${A.aliceWork}&folder=Sent`),
    g('/api/mail/conversations?unreadOnly=1'),
    g('/api/mail/conversations?search=invoice'),
    g('/api/mail/conversations?category=primary&unifiedInbox=1'),
    { ...g('/api/mail/conversations?limit=1'), capture: (j) => ({ cursor: j.nextCursor }) },
    (c) => g(`/api/mail/conversations?limit=1&cursor=${c.cursor}`),
    g('/api/mail/conversations?cursor=%%%'),
    g(`/api/mail/conversations/${C(1)}`),
    g(`/api/mail/conversations/${C(4)}`),
    g('/api/mail/conversations/not-a-uuid'),
    g(`/api/mail/conversations/${C(1)}/logical-messages/${L(1)}/body`),
    g(`/api/mail/conversations/${C(1)}/logical-messages/${L(1)}/body?remoteImages=1`, { headers: { 'X-MailFlow-Image-Opt-In': '1' } }),
    g(`/api/mail/conversations/${C(1)}/logical-messages/${L(8)}/body`),
    g(`/api/mail/conversations/${C(1)}/logical-messages/bad/body`),
    g(`/api/mail/messages/${M(1)}/conversation`),
    g(`/api/mail/messages/${M(4)}/conversation`),
    g('/api/mail/messages/%3Cm2%40fixture.test%3E/conversation'),
    g(`/api/mail/messages/%3Cm3%40fixture.test%3E/conversation?accountId=${A.aliceWork}`),
    g('/api/mail/messages/m1@fixture.test/conversation?accountId=nope'),
    g(`/api/mail/conversations/${C(1)}/diagnostics`),
    g('/api/mail/conversations/rebuild'),
    p(`/api/mail/conversations/${C(1)}/read`, { scope: 'WHOLE_CONVERSATION', isRead: true }),
    p(`/api/mail/conversations/${C(1)}/star`, { scope: 'EVERYTHING', isStarred: true }),
    p(`/api/mail/conversations/${C(1)}/move`, { scope: 'THIS_COPY' }),
    p(`/api/mail/conversations/${C(1)}/move`, { scope: 'THIS_COPY', copyId: M(2), targetFolder: 'Nope' }),
    p(`/api/mail/conversations/${C(4)}/read`, { isRead: true }),
    // RETURNING order is heap order, which is incidental; the after-state checks the rows.
    p('/api/mail/conversations/bulk-read', { conversationIds: [C(1), C(3), C(1)], scope: 'WHOLE_CONVERSATION', isRead: false }, { ignore: ['affectedIds'] }),
    p('/api/mail/conversations/bulk-read', { scope: 'WHOLE_CONVERSATION' }),
    p('/api/mail/conversations/bulk-move', { conversationIds: [C(1)], targetFolder: 'Sent' }),
    p(`/api/mail/conversations/${C(1)}/lock`),
    p(`/api/mail/conversations/${C(1)}/unlock`),
    p(`/api/mail/conversations/${C(1)}/merge`, {}),
    p(`/api/mail/conversations/${C(2)}/merge`, { targetConversationId: C(1) }),
    g(`/api/mail/conversations/${C(2)}`),
    p(`/api/mail/conversations/${C(1)}/merge`, { targetConversationId: C(3) }),
    p(`/api/mail/conversations/${C(1)}/logical-messages/${L(2)}/split`, { includeReplies: true }),
    p(`/api/mail/conversations/${C(1)}/logical-messages/${L(3)}/move`, { targetConversationId: C(1) }),
    p(`/api/mail/conversations/${C(1)}/logical-messages/${L(1)}/force-exclude`),
    p(`/api/mail/conversations/${C(1)}/overrides`, { overrideType: 'nope' }),
    p(`/api/mail/conversations/${C(1)}/overrides`, { overrideType: 'lock-conversation', reason: 7 }),
    p(`/api/mail/conversations/${C(1)}/overrides`, { overrideType: 'lock-conversation', logicalMessageId: L(1) }),
    p(`/api/mail/conversations/${C(4)}/overrides`, { overrideType: 'lock-conversation' }),
    p(`/api/mail/conversations/${C(1)}/overrides`, { overrideType: 'lock-conversation', reason: 'pin' }),
    g(`/api/mail/conversations/${C(1)}/overrides`),
    g(`/api/mail/conversations/${C(1)}`),
    p('/api/mail/conversations/rebuild', { accountId: A.bob }),
    p('/api/mail/conversations/rebuild', { accountId: A.aliceWork, dryRun: true }),
    // A third request inside the minute is 429 in Rust; Node's global handler drops the
    // limiter's statusCode and answers 500, so it is not compared here.
    g('/api/mail/conversations/rebuild/nope'),
    g(`/api/mail/conversations/rebuild/${C(9)}`),
  ],
  after: async ({ sql }) => ({
    messages: (await sql(`SELECT id, conversation_id, logical_message_id, is_read FROM messages WHERE account_id IN ($1, $2) ORDER BY id`, [A.aliceWork, A.aliceHome])).rows,
    conversations: (await sql(`SELECT id, kind, manually_locked, logical_message_count, copy_count, unread_count FROM conversations WHERE user_id = $1 AND kind <> 'manual_conversation' ORDER BY id`, [U.alice])).rows,
    aliases: (await sql(`SELECT alias_conversation_id, canonical_conversation_id, reason FROM conversation_aliases ORDER BY 1`)).rows,
    overrides: (await sql(`SELECT override_type, reason FROM conversation_overrides ORDER BY created_at, override_type`)).rows,
  }),
}];
