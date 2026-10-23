import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const admin = readFileSync(new URL('./AdminPanel.tsx', import.meta.url), 'utf8');
const settings = readFileSync(new URL('./ImapLimitsSettings.tsx', import.meta.url), 'utf8');
const api = readFileSync(new URL('../utils/api.ts', import.meta.url), 'utf8');

test('the IMAP connections sub-tab is admin-only and searchable', () => {
  assert.match(admin, /\.\.\.\(isAdmin \? \[\{ id: 'imap-connections'/);
  assert.match(admin, /subtab: 'imap-connections', adminOnly: true/);
});

test('admins can set the start value, toggle auto-lower and reset servers and accounts', () => {
  assert.match(settings, /api\.admin\.imapLimits\.updateServer\(server\.host, server\.port, \{ startLimit: parsed, autoLower \}\)/);
  assert.match(settings, /api\.admin\.imapLimits\.resetServer\(/);
  assert.match(settings, /api\.admin\.imapLimits\.resetAccount\(/);
  assert.match(settings, /resetConfirm/, 'server reset asks for confirmation');
  assert.match(api, /'\/admin\/imap-limits'/);
});
