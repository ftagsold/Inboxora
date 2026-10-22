import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { formatInvitationRange } from './invitationTime.ts';

describe('formatInvitationRange', () => {
  it('shows a one-day all-day invitation as its date', () => {
    assert.equal(formatInvitationRange({ allDay: true, startsAt: '2026-10-22T00:00:00.000Z', endsAt: '2026-10-23T00:00:00.000Z' }, 'en'), 'Oct 22, 2026');
  });

  it('shows a multi-day all-day invitation up to its last day, not the exclusive end', () => {
    assert.equal(formatInvitationRange({ allDay: true, startsAt: '2026-10-22T00:00:00.000Z', endsAt: '2026-10-24T00:00:00.000Z' }, 'en'), 'Oct 22, 2026 – Oct 23, 2026');
  });
});
