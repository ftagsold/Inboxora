import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';

const read = (name: string) => readFileSync(new URL(`./${name}`, import.meta.url), 'utf8');

test('message toolbars offer AI entries only while a provider is configured', () => {
  const conversation = read('ConversationMessage.tsx');
  assert.match(conversation, /const availableAiActions = aiStatus\?\.enabled && aiStatus\.features\?\.summarize && body\s*\?/);
  assert.match(conversation, /onManageAiActions=\{aiStatus\?\.enabled \?/);
  const pane = read('MessagePane.tsx');
  assert.match(pane, /aiActions=\{aiStatus\?\.enabled && aiStatus\?\.features\?\.summarize && body/);
  assert.match(pane, /onManageAiActions=\{aiStatus\?\.enabled \?/);
  const compose = read('ComposeModal.tsx');
  assert.match(compose, /aiEnabled=\{!htmlMode && aiStatus\?\.enabled && aiStatus\?\.features\?\.compose\}/);
  // MessageToolbar renders the AI button only for a non-empty list and the manage entry only with a handler.
  const toolbar = read('MessageToolbar.tsx');
  assert.match(toolbar, /\{aiActions\.length > 0 && <div/);
  assert.match(toolbar, /\{onManageAiActions && <MenuItem/);
});

test('settings hide the AI actions tab and its search results without a provider', () => {
  const admin = read('AdminPanel.tsx');
  assert.match(admin, /\(tab\.id !== 'ai-actions' \|\| aiActionsAvailable\)/);
  assert.match(admin, /adminTab === 'ai-actions' && aiActionsAvailable && <AiActionsTab \/>/);
  assert.match(admin, /item\.tab === 'ai-actions' && !aiActionsAvailable/);
});
