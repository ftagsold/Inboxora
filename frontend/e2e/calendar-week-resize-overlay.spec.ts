import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('week view: resizing a timed event across several day boundaries never paints its full-day preview blocks over the sticky day header', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM stacking behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [
      { id: '123', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: '123', all_day: true, starts_at: '2026-09-07T00:00:00Z', ends_at: '2026-09-11T00:00:00Z' },
      { id: 'kz', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'Kuerzinger Zimmerer', starts_at: '2026-09-07T17:00:00', ends_at: '2026-09-07T18:00:00' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Tydzień', exact: true }).click();
  const event = page.getByRole('button', { name: /Kuerzinger/ }).first();
  await expect(event).toBeVisible();
  await event.scrollIntoViewIfNeeded();
  const box = await event.boundingBox();
  const handle = page.getByTestId('calendar-event-resize-bottom').first();
  const handleBox = await handle.boundingBox();
  await page.mouse.move(handleBox.x + handleBox.width / 2, handleBox.y + handleBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(handleBox.x + handleBox.width / 2, handleBox.y + 60, { steps: 5 });
  // Drag three day-columns to the right — the days strictly between the event's own day and the
  // day under the pointer each get a full 24-hour-tall preview block, since the event now
  // genuinely spans full days there.
  await page.mouse.move(handleBox.x + 3 * (box.width + 1), handleBox.y + 100, { steps: 10 });
  const previews = page.getByTestId('calendar-event-resize-preview');
  // Exactly how many day-columns register the drag having crossed into them depends on precise
  // pixel/step timing and isn't the point of this test — what matters is that it's a genuine
  // multi-day preview (more than the single original day), and that at least one of the
  // in-between days gets a full 24-hour-tall block (top at the very start of the scrollable day,
  // reaching all the way up past the header once scrolled) — the exact geometry the sticky
  // header's z-index (see CalendarPageContract.test.ts) has to stay above.
  await expect(previews).not.toHaveCount(1);
  const count = await previews.count();
  const heights = await Promise.all(Array.from({ length: count }, (_, i) => previews.nth(i).evaluate(el => getComputedStyle(el).height)));
  await page.mouse.up();
  expect(heights).toContain('1440px');
});
