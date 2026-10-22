// POST /api/mail/send through the SMTP sink (validation, delivery, idempotent replay, recipient
// learning) and the draft routes' validation. The fixture IMAP hosts do not resolve, so the Sent
// APPEND and draft APPEND fail and report it.
import { createCipheriv, randomBytes } from 'node:crypto';
import { A, M, U, seedMail } from '../seed-mail.mjs';
import { sink } from '../smtp-sink.mjs';

const SINK = { node: sink(2555), rust: sink(2556) };
const KEY = Buffer.from('1'.repeat(64), 'hex');

function encrypt(text) {
  const iv = randomBytes(12);
  const c = createCipheriv('aes-256-gcm', KEY, iv);
  const enc = Buffer.concat([c.update(text, 'utf8'), c.final()]);
  return `enc:v1:${iv.toString('hex')}:${c.getAuthTag().toString('hex')}:${enc.toString('hex')}`;
}

async function seed(ctx) {
  const out = await seedMail(ctx);
  const { sql, backend } = ctx;
  for (const key of ['allow_private_hosts', 'allow_insecure_tls', 'allow_nonstandard_ports']) {
    await sql(`INSERT INTO system_settings (key, value) VALUES ($1, 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'`, [key]);
  }
  await sql(`UPDATE email_accounts SET smtp_host = '127.0.0.1', smtp_port = $2, smtp_tls = 'none', smtp_auth_user = 'alice', smtp_auth_pass = $3,
    sender_name = 'Alice Work', signature = '<p>Regards, <b>Alice</b></p>' WHERE id = $1`, [A.aliceWork, SINK[backend].port, encrypt('pw')]);
  await sql(`UPDATE messages SET attachments = $2::jsonb WHERE id = $1`, [M(1), JSON.stringify([{ part: '2', filename: 'invoice.pdf', type: 'application/pdf', size: 30_000_000 }])]);
  SINK[backend].messages.length = 0;
  return { ...out, sink: backend };
}

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const send = (body, extra = {}) => ({ method: 'POST', path: '/api/mail/send', as: 'alice', body, ...extra });
const draft = (body, extra = {}) => ({ method: 'POST', path: '/api/mail/draft', as: 'alice', body, ...extra });
const ok = { accountId: A.aliceWork, to: ['Bob <bob@x.test>'] };
const message = {
  ...ok,
  cc: ['carol@corp.test'],
  bcc: ['hidden@corp.test'],
  subject: 'Hello\r\nBcc: evil@x.test',
  body: 'Line one\nLine <two>',
  priority: 'high',
  attachments: [{ filename: 'note.txt', content: Buffer.from('attached text').toString('base64'), contentType: 'text/plain' }],
};

export default [{
  name: 'send: validation, delivery, idempotency and drafts',
  seed,
  sessions,
  ignoreKeys: ['created_at', 'updated_at'],
  steps: [
    send({ to: ['a@b.c'] }),
    send({ ...ok, bodyIsHtml: 'yes' }),
    send({ ...ok, editedSignatureIsHtml: 1 }),
    send({ ...ok, attachments: {} }),
    send({ ...ok, attachments: [{ content: 'eA==' }] }),
    send({ ...ok, attachments: [{ filename: 'x', content: 1 }] }),
    send({ ...ok, forwardedAttachments: [{ messageId: 'nope', part: '1' }] }),
    send({ ...ok, forwardedAttachments: [{ messageId: M(1), part: ' ' }] }),
    send({ ...ok, forwardedAttachments: [{ messageId: M(1), part: '2' }] }),
    send({ ...ok, forwardedAttachments: [{ messageId: M(1), part: '9' }] }),
    send({ ...ok, forwardedAttachments: [{ messageId: M(7), part: '1' }] }),
    send({ ...ok, to: 'bob@x.test' }),
    send({ ...ok, to: ['bob@'] }),
    send({ ...ok, to: ['', 'x@y.z'] }),
    send({ ...ok, to: ['a@b.c\nBcc: x@y.z'] }),
    send({ accountId: A.aliceWork, to: [] }),
    send({ ...ok, accountId: A.bob }),
    send({ ...ok, aliasId: '00000000-0000-4000-8000-00000000ffff' }),
    send(message, { headers: { 'X-Idempotency-Key': 'key-1' } }),
    send(message, { headers: { 'X-Idempotency-Key': 'key-1' } }),
    send({ ...message, subject: 'Changed' }, { headers: { 'X-Idempotency-Key': 'key-1' } }),
    send({ ...ok, body: '<p>Hi</p>', bodyIsHtml: true, editedSignature: 'Plain sig', editedSignatureIsHtml: false, inReplyTo: '<m2@fixture.test>' }),
    draft({ accountId: A.aliceWork, editedSignatureIsHtml: 'no' }),
    draft({ to: ['x@y.z'] }),
    draft({ accountId: A.bob }),
    draft({ accountId: A.aliceWork, aliasId: '00000000-0000-4000-8000-00000000ffff' }),
    draft({ accountId: A.aliceWork, to: ['x@y.z'], subject: 'Draft' }),
    { method: 'DELETE', path: '/api/mail/draft/abc', as: 'alice' },
    { method: 'DELETE', path: '/api/mail/draft/5', as: 'alice' },
    { method: 'DELETE', path: `/api/mail/draft/5?accountId=${A.aliceWork}&folder=Drafts&uidValidity=1.5`, as: 'alice' },
    { method: 'DELETE', path: `/api/mail/draft/5?accountId=${A.bob}&folder=Drafts&uidValidity=7`, as: 'alice' },
    { method: 'DELETE', path: `/api/mail/draft/5?accountId=${A.aliceWork}&folder=Drafts&uidValidity=7`, as: 'alice' },
  ],
  after: async ({ sql, ctx }) => {
    const mails = SINK[ctx.sink].messages.map((m) => {
      const head = m.data.split('\r\n\r\n')[0].replace(/\r\n[ \t]+/g, ' ');
      const header = (name) => head.match(new RegExp(`^${name}: (.*)$`, 'mi'))?.[1] ?? null;
      return {
        from: m.from, to: [...m.to].sort(), subject: header('Subject'), priority: header('X-Priority'), importance: header('Importance'),
        hasBcc: /^Bcc:/mi.test(head), inReplyTo: header('In-Reply-To'), references: header('References'),
        attachment: m.data.includes('note.txt'), text: /Line one/.test(SINK[ctx.sink].lastText()) || /Hi/.test(m.data),
      };
    });
    return {
      mails,
      contacts: (await sql(`SELECT display_name, primary_email, send_count, is_auto FROM contacts WHERE user_id = $1 AND primary_email IN ('bob@x.test', 'carol@corp.test', 'hidden@corp.test') ORDER BY primary_email`, [U.alice])).rows,
      intents: (await sql(`SELECT idempotency_key, status, result FROM send_idempotency ORDER BY idempotency_key`)).rows,
    };
  },
}];
