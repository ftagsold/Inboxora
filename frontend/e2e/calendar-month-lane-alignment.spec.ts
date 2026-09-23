import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// Reproduces the exact shape of a user-reported bug: two multi-day all-day events overlap for
// part of their range, and one of the days they share is much busier (several extra single-day
// and timed events) than the others. Each day cell used to sort its own event list completely
// independently, so a busier neighbor could push a spanning event onto a different row than the
// same event occupies in a quieter cell right next to it — reading as the bar "breaking" even
// though every individual segment, in isolation, was rendered correctly. Both events now share
// one lane assignment for the whole week (see monthWeekLanes in CalendarPage.tsx), so they must
// land on the exact same row in every cell they touch regardless of what else is on any one day.
test('month view: two overlapping multi-day events stay aligned on the same row across every day, even where one day is much busier than its neighbors', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM layout behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [
      // Spans Mon(7)-Thu(10) — four days.
      { id: 'first', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'first', all_day: true, starts_at: '2026-09-07T00:00:00Z', ends_at: '2026-09-11T00:00:00Z' },
      // Spans Wed(9)-Fri(11) — overlaps "first" on Wed(9) and Thu(10).
      { id: 'second', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'second', all_day: true, starts_at: '2026-09-09T00:00:00Z', ends_at: '2026-09-12T00:00:00Z' },
      // Thursday(10) alone gets three extra events — a busier neighbor that must not push
      // "first"/"second" off their shared row there.
      { id: 'birthday', calendar_id: 'calendar-personal', calendar_color: '#35793a', source: 'local', read_only: false, summary: 'Birthday', all_day: true, starts_at: '2026-09-10T00:00:00Z', ends_at: '2026-09-11T00:00:00Z' },
      { id: 'trash', calendar_id: 'calendar-personal', calendar_color: '#9c7a4a', source: 'local', read_only: false, summary: 'Trash', starts_at: '2026-09-10T06:00:00Z', ends_at: '2026-09-10T06:15:00Z' },
      { id: 'evening', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'Evening shift', starts_at: '2026-09-10T17:00:00Z', ends_at: '2026-09-10T17:30:00Z' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  await expect(page.getByTestId('calendar-month-grid')).toBeVisible();

  const cells = page.locator('[data-testid="calendar-month-grid"] .cal-cell');
  const cellCount = await cells.count();
  // The row a segment lands on is measured by its actual on-screen vertical offset from its own
  // cell's top edge (not by counting `.cal-ev` elements) — a skipped lane still occupies a row
  // via a blank spacer (not a `.cal-ev`), so only comparing real screen position reflects what a
  // viewer actually sees regardless of how any given lane happens to be filled.
  const topByDate: Record<string, { first?: number; second?: number }> = {};
  for (let i = 0; i < cellCount; i += 1) {
    const cell = cells.nth(i);
    const dateLabel = (await cell.locator('.calendar-day-select').first().textContent().catch(() => ''))?.trim() ?? '';
    const cellBox = await cell.boundingBox();
    if (!cellBox) continue;
    const firstEl = cell.locator('.cal-ev', { hasText: 'first' }).first();
    const secondEl = cell.locator('.cal-ev', { hasText: 'second' }).first();
    const firstBox = (await firstEl.count()) ? await firstEl.boundingBox() : null;
    const secondBox = (await secondEl.count()) ? await secondEl.boundingBox() : null;
    if (firstBox || secondBox) {
      topByDate[dateLabel] = {
        first: firstBox ? Math.round(firstBox.y - cellBox.y) : undefined,
        second: secondBox ? Math.round(secondBox.y - cellBox.y) : undefined,
      };
    }
  }

  // "first" (days 7,8,9,10) sits at the same vertical offset in every one of its cells, busy
  // Thursday included.
  const firstTops = ['7', '8', '9', '10'].map(day => topByDate[day]?.first);
  expect(firstTops.every(top => top === firstTops[0])).toBe(true);
  expect(firstTops[0]).not.toBeUndefined();

  // "second" (days 9,10,11) is likewise at one consistent vertical offset throughout — including
  // day 11, where "first" (whose lane it shares on 9 and 10) isn't present at all.
  const secondTops = ['9', '10', '11'].map(day => topByDate[day]?.second);
  expect(secondTops.every(top => top === secondTops[0])).toBe(true);
  expect(secondTops[0]).not.toBeUndefined();

  // And they don't collide with each other on the two days they share.
  expect(topByDate['9']?.first).not.toBe(topByDate['9']?.second);
  expect(topByDate['10']?.first).not.toBe(topByDate['10']?.second);
});
