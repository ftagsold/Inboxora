import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import {
  DEFAULT_LOCAL_AI_MODEL, LOCAL_AI_MODELS, isKnownLocalAiModel, localAiActive, setLocalAiModel,
  stripThinking, toLocalMessages,
} from './localAi.ts';

test('the default local model is Gemma 4 and every curated id is known', () => {
  assert.equal(DEFAULT_LOCAL_AI_MODEL, 'gemma-4-E2B-it-q4f16_1-MLC');
  for (const m of LOCAL_AI_MODELS) assert.ok(isKnownLocalAiModel(m.id));
  assert.equal(isKnownLocalAiModel('evil-model'), false);
  assert.equal(isKnownLocalAiModel(42), false);
});

test('system prompts fold into the first user turn', () => {
  const out = toLocalMessages([
    { role: 'system', content: 'Fix spelling.' },
    { role: 'user', content: 'Helo wrld' },
  ]);
  assert.deepEqual(out, [{ role: 'user', content: 'Fix spelling.\n\nHelo wrld' }]);
});

test('the last turn is capped to fit the model context', () => {
  const out = toLocalMessages([{ role: 'user', content: 'x'.repeat(20000) }]);
  assert.equal(out[0].content.length, 6000);
});

test('reasoning blocks are stripped, also while still open', () => {
  assert.equal(stripThinking('<think>hmm</think>\n\nAnswer'), 'Answer');
  assert.equal(stripThinking('<think>still thinking'), '');
  assert.equal(stripThinking('Plain'), 'Plain');
});

test('local AI stays inactive without WebGPU', () => {
  setLocalAiModel(DEFAULT_LOCAL_AI_MODEL);
  assert.equal(localAiActive(), false);
  setLocalAiModel(null);
  assert.equal(localAiActive(), false);
});

test('api.ai.chat routes to the local model while it is active', () => {
  const api = readFileSync(new URL('./api.ts', import.meta.url), 'utf8');
  assert.match(api, /localAiActive\(\) \? streamLocalAiChat\(messages, opts\) : streamAiChat\(messages, opts\)/);
});
