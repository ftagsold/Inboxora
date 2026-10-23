import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { bodyLoadOrder, initialConversationExpansion, initialConversationTarget, toggleConversationExpansion } from './conversationExpansion.ts';

const messages = [{ id: 'M1' }, { id: 'M2' }, { id: 'M3' }];

describe('conversation expansion policy', () => {
  it('initially expands every message, the selected one first', () => {
    const expanded = initialConversationExpansion(messages, 'M2');
    assert.deepEqual([...expanded], ['M2', 'M1', 'M3']);
  });

  it('falls back to the newest message as target when the target is stale', () => {
    assert.equal(initialConversationTarget(messages, 'stale'), 'M3');
    assert.deepEqual([...initialConversationExpansion(messages, 'stale')], ['M3', 'M1', 'M2']);
  });

  it('expands nothing for an empty conversation', () => {
    assert.equal(initialConversationExpansion([], 'M1').size, 0);
  });

  it('collapsing one message leaves the others open', () => {
    const expanded = toggleConversationExpansion(initialConversationExpansion(messages, 'M3'), 'M1');
    assert.deepEqual([...expanded], ['M3', 'M2']);
  });

  it('allows every card to be manually collapsed', () => {
    let expanded = initialConversationExpansion(messages, 'M3');
    for (const id of ['M1', 'M2', 'M3']) expanded = toggleConversationExpansion(expanded, id);
    assert.equal(expanded.size, 0);
  });

  it('loads the target body first, then its neighbours', () => {
    const five = [{ id: 'A' }, { id: 'B' }, { id: 'C' }, { id: 'D' }, { id: 'E' }];
    assert.deepEqual(bodyLoadOrder(['A', 'B', 'C', 'D', 'E'], five, 'C'), ['C', 'D', 'B', 'E', 'A']);
    assert.deepEqual(bodyLoadOrder(['A', 'E'], five, 'E'), ['E', 'A']);
    assert.deepEqual(bodyLoadOrder(['A', 'B'], five, null), ['A', 'B']);
  });
});
