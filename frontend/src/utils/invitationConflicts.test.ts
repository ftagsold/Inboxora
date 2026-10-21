import assert from 'node:assert/strict';
import test from 'node:test';
import { invitationConflicts } from './invitationConflicts.ts';

const invitation = { startsAt: '2026-09-11T13:00:00Z', endsAt: '2026-09-11T14:00:00Z', allDay: false };
const event = (id: string, starts: string, ends: string, extra: Record<string, unknown> = {}) => ({ id, summary: id, starts_at: starts, ends_at: ends, ...extra });

test('reports overlapping events in start order and ignores touching ones', () => {
  const conflicts = invitationConflicts(invitation, [
    event('late', '2026-09-11T13:30:00Z', '2026-09-11T15:00:00Z'),
    event('early', '2026-09-11T12:00:00Z', '2026-09-11T13:15:00Z'),
    event('before', '2026-09-11T12:00:00Z', '2026-09-11T13:00:00Z'),
    event('after', '2026-09-11T14:00:00Z', '2026-09-11T15:00:00Z'),
  ], 'message-1');
  assert.deepEqual(conflicts.map(item => item.id), ['early', 'late']);
});

test('never reports the invitation against its own calendar copy, contact dates or all-day days', () => {
  const conflicts = invitationConflicts(invitation, [
    event('own copy', '2026-09-11T13:00:00Z', '2026-09-11T14:00:00Z', { source_message_id: 'message-1' }),
    event('birthday', '2026-09-11T00:00:00Z', '2026-09-12T00:00:00Z', { calendar_id: 'contacts-birthdays', all_day: true }),
    event('holiday', '2026-09-11T00:00:00Z', '2026-09-12T00:00:00Z', { all_day: true }),
  ], 'message-1');
  assert.deepEqual(conflicts, []);
});

test('an all-day invitation collides with anything on that day', () => {
  const conflicts = invitationConflicts({ startsAt: '2026-09-11T00:00:00Z', endsAt: '2026-09-12T00:00:00Z', allDay: true }, [
    event('meeting', '2026-09-11T09:00:00Z', '2026-09-11T10:00:00Z'),
    event('trip', '2026-09-10T00:00:00Z', '2026-09-13T00:00:00Z', { all_day: true }),
  ], 'message-1');
  assert.deepEqual(conflicts.map(item => item.id), ['trip', 'meeting']);
});

test('an invitation without a valid range has no conflicts', () => {
  assert.deepEqual(invitationConflicts({ startsAt: 'nope', endsAt: null }, [event('x', '2026-09-11T13:00:00Z', '2026-09-11T14:00:00Z')], 'm'), []);
});
