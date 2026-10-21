import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./db.js', () => ({ withTransaction: vi.fn(), query: vi.fn() }));
vi.mock('./conversationOverridePolicy.js', () => ({
  resolveConversationAlias: vi.fn(async (_client, { conversationId }) => conversationId),
}));
vi.mock('../utils/mailUtils.js', () => ({
  adjustFolderCounts: vi.fn(),
  resolveTrashFolder: vi.fn(async () => 'Trash'),
  resolveAllTrashPaths: vi.fn(async () => new Set(['Trash'])),
  resolveAllDraftsPaths: vi.fn(async () => new Set(['Drafts'])),
}));

import { applyBulkConversationAction, applyConversationAction } from './conversationActions.js';
import { query as __mock_query, withTransaction as __mock_withTransaction } from './db.js';
import { resolveTrashFolder } from '../utils/mailUtils.js';
import { mockPoolClient } from '../test/poolClient.js';

const withTransaction = vi.mocked(__mock_withTransaction);
const query = vi.mocked(__mock_query);
const account = { id: 'account-1', folder_mappings: {} };

// One INBOX copy (uid 7) unless a folder is given.
function fakeClient(folder = 'INBOX') {
  const calls: Array<{ sql: string; params: unknown[] }> = [];
  return {
    calls,
    async query(sql: string, params: unknown[]) {
      calls.push({ sql, params });
      if (sql.includes('SELECT account_id FROM conversations')) return { rows: [{ account_id: 'account-1' }] };
      if (sql.includes('SELECT m.id, m.account_id, m.logical_message_id, m.conversation_id')) {
        return { rows: [{ id: 'copy-1', account_id: 'account-1', logical_message_id: 'logical-1', conversation_id: 'conversation-1', folder, uid: 7, is_read: false }] };
      }
      if (sql.includes('SELECT * FROM email_accounts')) return { rows: [account] };
      if (sql.includes('UPDATE messages SET is_read')) return { rows: [{ id: 'copy-1', is_read: true }], rowCount: 1 };
      if (sql.includes('UPDATE messages SET is_starred')) return { rows: [{ id: 'copy-1', is_starred: true }], rowCount: 1 };
      return { rows: [], rowCount: 0 };
    },
  };
}

function fakeImap() {
  return {
    broadcast: vi.fn(),
    syncFolderOnDemand: vi.fn(async () => {}),
    bulkMoveMessages: vi.fn(async (_account: unknown, uids: unknown[]) => ({ succeeded: uids.map(String), uidMap: new Map([[7, 42]]) })),
    bulkPermanentDelete: vi.fn(async (_account: unknown, uids: unknown[]) => ({ succeeded: uids.map(String), failed: [] })),
    setFlag: vi.fn(async () => {}),
    _enqueueFlagPush: vi.fn(),
    _resolveFlagPush: vi.fn(),
  };
}

const run = (client: ReturnType<typeof fakeClient>) => withTransaction.mockImplementationOnce(async fn => fn(mockPoolClient(client)));

beforeEach(() => {
  withTransaction.mockReset();
  query.mockReset().mockResolvedValue({ rows: [account] });
  vi.mocked(resolveTrashFolder).mockResolvedValue('Trash');
});

describe('conversation delete goes through the mail server', () => {
  it('moves an inbox copy to Trash on the server and re-keys the row there', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    const result = await applyConversationAction({ userId: 'user-1', conversationId: 'conversation-1', copyId: 'copy-1', action: 'delete', imapManager: imap });
    expect(imap.bulkMoveMessages).toHaveBeenCalledWith(account, [7], 'INBOX', 'Trash');
    expect(client.calls.find(call => call.sql.startsWith('UPDATE messages SET folder'))?.params).toEqual(['Trash', 42, 'copy-1']);
    expect(client.calls.some(call => call.sql.includes('is_deleted = true'))).toBe(false);
    expect(result).toMatchObject({ ok: true, affectedIds: ['copy-1'] });
  });

  it('permanently deletes a copy that is already in Trash', async () => {
    const client = fakeClient('Trash'); const imap = fakeImap(); run(client);
    await applyConversationAction({ userId: 'user-1', conversationId: 'conversation-1', copyId: 'copy-1', action: 'delete', imapManager: imap });
    expect(imap.bulkPermanentDelete).toHaveBeenCalledWith(account, [7], 'Trash');
    expect(imap.bulkMoveMessages).not.toHaveBeenCalled();
    expect(client.calls.some(call => call.sql === 'DELETE FROM messages WHERE id = $1')).toBe(true);
  });

  it('fails without touching the row when the server move fails', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    imap.bulkMoveMessages.mockResolvedValueOnce({ succeeded: [], uidMap: new Map() });
    await expect(applyConversationAction({ userId: 'user-1', conversationId: 'conversation-1', copyId: 'copy-1', action: 'delete', imapManager: imap }))
      .rejects.toMatchObject({ statusCode: 502 });
    expect(client.calls.some(call => /^(UPDATE messages SET folder|DELETE FROM messages)/.test(call.sql))).toBe(false);
  });

  it('refuses to delete when the account has no Trash folder', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    vi.mocked(resolveTrashFolder).mockResolvedValueOnce(null);
    await expect(applyConversationAction({ userId: 'user-1', conversationId: 'conversation-1', copyId: 'copy-1', action: 'delete', imapManager: imap }))
      .rejects.toMatchObject({ statusCode: 422 });
    expect(imap.bulkMoveMessages).not.toHaveBeenCalled();
  });

  it('bulk delete takes the same server path', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    await applyBulkConversationAction({ userId: 'user-1', items: [{ conversationId: 'conversation-1', copyId: 'copy-1' }], scope: 'THIS_COPY', action: 'delete', imapManager: imap });
    expect(imap.bulkMoveMessages).toHaveBeenCalledWith(account, [7], 'INBOX', 'Trash');
  });
});

describe('conversation read/star are written to the mail server', () => {
  it('pushes \\Seen after the read commits', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    await applyConversationAction({ userId: 'user-1', conversationId: 'conversation-1', copyId: 'copy-1', action: 'read', isRead: true, imapManager: imap });
    expect(imap.setFlag).toHaveBeenCalledWith(account, 7, 'INBOX', '\\Seen', true);
    expect(imap._resolveFlagPush).toHaveBeenCalledWith('account-1', 'copy-1', '\\Seen');
  });

  it('queues a durable retry when the flag push fails', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    imap.setFlag.mockRejectedValueOnce(new Error('offline'));
    vi.spyOn(console, 'error').mockImplementationOnce(() => {});
    await applyConversationAction({ userId: 'user-1', conversationId: 'conversation-1', copyId: 'copy-1', action: 'star', isStarred: true, imapManager: imap });
    expect(imap._enqueueFlagPush).toHaveBeenCalledWith('account-1', 'copy-1', '\\Flagged', true);
  });

  it('bulk read/unread pushes and broadcasts every affected copy', async () => {
    const client = fakeClient(); const imap = fakeImap(); run(client);
    await applyBulkConversationAction({ userId: 'user-1', items: [{ conversationId: 'conversation-1', copyId: 'copy-1' }], scope: 'THIS_COPY', action: 'read', isRead: false, imapManager: imap });
    expect(imap.setFlag).toHaveBeenCalledWith(account, 7, 'INBOX', '\\Seen', false);
    expect(imap.broadcast).toHaveBeenCalledWith({ type: 'message_flags', accountId: 'account-1', changes: [{ id: 'copy-1', is_read: false }] }, 'user-1');
  });
});
