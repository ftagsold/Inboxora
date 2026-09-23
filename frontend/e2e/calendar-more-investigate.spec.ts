import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: dragging a multi-day event forward by less than its own length keeps the preview\'s title visible and its own leading edge rounded, even while overlapping the ghost', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [{ id: '123', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: '123', all_day: true, starts_at: '2026-09-15T00:00:00Z', ends_at: '2026-09-18T00:00:00Z' }] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: '123' }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  // Forward by 1 day — the preview's own genuine first day (the 16th) is also the first day the
  // ghost's own segment is suppressed on, in favor of the preview. The preview's leading edge
  // rounds off here regardless — it's its own true first day, and it never squares against the
  // ghost sitting right next to it (the two are never merged into one bar).
  await page.mouse.move(box.x + box.width / 2 + (box.width + 1), box.y + box.height / 2, { steps: 10 });
  const previews = page.getByTestId('calendar-month-move-preview');
  const firstPreviewInner = previews.first().locator('div').first();
  const titleSpan = firstPreviewInner.locator('span').first();
  await expect(titleSpan).toBeVisible();
  await expect(titleSpan).toHaveText('123');
  const radius = await firstPreviewInner.evaluate(el => getComputedStyle(el).borderTopLeftRadius);
  await page.mouse.up();
  expect(radius).toBe('4px');
});
