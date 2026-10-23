import { test } from 'node:test';
import assert from 'node:assert/strict';
import { combine, dateKey, monthGrid, schedulePresets, timeKey } from './scheduleSend.ts';

test('presets: later today only while it stays today and before 21:00', () => {
  const morning = schedulePresets(new Date(2026, 9, 21, 9, 10)); // Wednesday
  assert.deepEqual(morning.map(p => p.key), ['laterToday', 'tomorrowMorning', 'tomorrowAfternoon', 'monday']);
  assert.equal(timeKey(morning[0].at), '12:30');
  const evening = schedulePresets(new Date(2026, 9, 21, 19, 0));
  assert.equal(evening[0].key, 'tomorrowMorning');
});

test('presets: next Monday is a week ahead on Mondays and skipped on Sundays (tomorrow covers it)', () => {
  const monday = schedulePresets(new Date(2026, 9, 19, 22, 0));
  assert.equal(dateKey(monday.find(p => p.key === 'monday')!.at), '2026-10-26');
  const sunday = schedulePresets(new Date(2026, 9, 25, 22, 0));
  assert.equal(sunday.some(p => p.key === 'monday'), false);
  assert.equal(dateKey(sunday[0].at), '2026-10-26');
});

test('combine parses local date and time and rejects garbage', () => {
  const d = combine('2026-10-24', '08:05');
  assert.ok(d);
  assert.equal(dateKey(d), '2026-10-24');
  assert.equal(timeKey(d), '08:05');
  assert.equal(combine('2026-10-24', ''), null);
  assert.equal(combine('24.10.2026', '08:00'), null);
});

test('month grid starts on Monday and covers six weeks', () => {
  const grid = monthGrid(new Date(2026, 9, 15));
  assert.equal(grid.length, 42);
  assert.equal(grid[0].getDay(), 1);
  assert.equal(dateKey(grid[0]), '2026-09-28');
  assert.ok(grid.some(d => dateKey(d) === '2026-10-31'));
});
