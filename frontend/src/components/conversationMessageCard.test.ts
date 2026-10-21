import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';

const read = (name: string) => readFileSync(new URL(`./${name}`, import.meta.url), 'utf8');

test('an expanded conversation message renders header, details and body inside one card', () => {
  const message = read('ConversationMessage.tsx');
  const card = message.indexOf('<div className="msg-card"');
  const expanded = message.indexOf('expanded && <div data-conversation-message-expanded-content');
  const headerModal = message.indexOf('{showHeaders && copy.id && <MessageHeaderModal');
  assert.ok(card > 0 && expanded > card, 'expanded content follows the card opening');
  // The card closes after the expanded content, right before the header modal.
  assert.match(message.slice(expanded, headerModal), /embedded\s*\/>\}\s*<\/div>\}\s*<\/div>\s*$/);
});

test('embedded detail content shares one inset and a uniform gap', () => {
  const detail = read('MessageDetailContent.tsx');
  assert.match(detail, /data-message-detail-details="true" style=\{\{ display: 'flex', flexDirection: 'column', gap: 12/);
  assert.doesNotMatch(detail, /data-message-detail-attachments="true" style=\{\{ marginBottom/);
  assert.doesNotMatch(detail, /className="msg-notice"[^>]*marginTop/);
});
