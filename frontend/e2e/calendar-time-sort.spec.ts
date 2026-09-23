import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: a dragged timed event\'s preview inserts by time-of-day, not its stale original day', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [
      // The dragged event's own original day (the 10th) is chronologically *before* the target
      // day (the 15th) — a naive absolute-timestamp comparison would always sort it first
      // there, regardless of its 10:30 time-of-day actually belonging between the two events
      // already on the 15th.
      { id: 'dragged', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'Dragged Event', starts_at: '2026-09-10T10:30:00Z', ends_at: '2026-09-10T11:00:00Z' },
      { id: 'early', calendar_id: 'calendar-personal', calendar_color: '#2e8b45', source: 'local', read_only: false, summary: 'Early Event', starts_at: '2026-09-15T09:00:00Z', ends_at: '2026-09-15T09:30:00Z' },
      { id: 'late', calendar_id: 'calendar-personal', calendar_color: '#2e8b45', source: 'local', read_only: false, summary: 'Late Event', starts_at: '2026-09-15T14:00:00Z', ends_at: '2026-09-15T14:30:00Z' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: /Dragged Event/ }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  // The visible grid starts Monday Aug 31 (index 0); the 10th is index 10, the 15th index 15 —
  // five day-cells to the right, same week row.
  const targetCell = page.locator('[data-testid="calendar-month-grid"] .cal-cell').nth(15);
  const targetBox = await targetCell.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 10 });
  const previewCell = page.getByTestId('calendar-month-move-preview');
  await expect(previewCell).toBeVisible();
  const cellTitles = await targetCell.locator('.cal-ev, [data-testid="calendar-month-move-preview"] > div').allTextContents();
  console.log('cell order during drag', JSON.stringify(cellTitles));
  await page.mouse.up();
  // 09:00 Early, 10:30 Dragged, 14:00 Late — ascending by time of day.
  expect(cellTitles.map(text => text.replace(/[\d:]+$/, '').trim())).toEqual(['Early Event', 'Dragged Event', 'Late Event']);
});
