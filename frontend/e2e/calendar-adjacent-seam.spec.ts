import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: dragging a multi-day event by exactly its own length rounds off both the ghost\'s trailing edge and the preview\'s leading edge — they never merge into one bar', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  // A 3-day event ("123", the 15th-17th) dragged forward by exactly 3 days lands on the 18th-
  // 20th — immediately adjacent to its own original range (the 17th ends where the 18th
  // begins), not overlapping it at all. The ghost and the preview are two visually distinct
  // things (dimmed original vs. bright new position); a user explicitly asked that neither one
  // ever square an edge off just because it happens to touch the other.
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
  await page.mouse.move(box.x + box.width / 2 + 3 * (box.width + 1), box.y + box.height / 2, { steps: 15 });
  const ghostLast = page.locator('[data-testid="calendar-month-span"] .cal-ev').last();
  const previewFirst = page.getByTestId('calendar-month-move-preview').locator('div').first();
  await expect(ghostLast).toBeVisible();
  await expect(previewFirst).toBeVisible();
  const ghostStyle = await ghostLast.evaluate(el => getComputedStyle(el).borderTopRightRadius);
  const previewStyle = await previewFirst.evaluate(el => getComputedStyle(el).borderTopLeftRadius);
  await page.mouse.up();
  expect(ghostStyle).toBe('4px');
  expect(previewStyle).toBe('4px');
});

test('month view: dragging a multi-day event so the preview overlaps the ghost still rounds both, and the ghost\'s exposed edge (where its suppressed neighbor used to be) rounds too', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  // Same 3-day event, but dragged forward by only 2 days: the preview (17th-19th) genuinely
  // overlaps the ghost's own range (15th-17th) on the 17th, rather than exactly touching it. The
  // 17th shows only the preview (the ghost's own segment there is suppressed), leaving the 16th
  // as the ghost's own last remaining visible day — its real next day (the 17th) isn't actually
  // rendered any more, so its trailing edge rounds off too, same as the preview's leading edge.
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
  await page.mouse.move(box.x + box.width / 2 + 2 * (box.width + 1), box.y + box.height / 2, { steps: 15 });
  const ghostLast = page.locator('[data-testid="calendar-month-span"] .cal-ev').last();
  const previewFirst = page.getByTestId('calendar-month-move-preview').locator('div').first();
  await expect(ghostLast).toBeVisible();
  await expect(previewFirst).toBeVisible();
  const ghostStyle = await ghostLast.evaluate(el => getComputedStyle(el).borderTopRightRadius);
  const previewStyle = await previewFirst.evaluate(el => getComputedStyle(el).borderTopLeftRadius);
  await page.mouse.up();
  expect(ghostStyle).toBe('4px');
  expect(previewStyle).toBe('4px');
});
