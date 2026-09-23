import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: hovering one segment of a multi-day event highlights every segment', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [{ id: 'span', calendar_id: 'calendar-personal', calendar_color: '#35548a', source: 'local', read_only: false, summary: 'Warsztaty wyjazdowe', all_day: true, starts_at: '2026-09-14T00:00:00Z', ends_at: '2026-09-18T00:00:00Z' }] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const segments = page.locator('[data-testid="calendar-month-span"] .cal-ev');
  await expect(segments).toHaveCount(4);
  const filters = () => segments.evaluateAll(els => els.map(el => getComputedStyle(el).filter));
  await segments.nth(2).hover(); // a middle (continuation) segment, not the first
  await expect.poll(async () => (await filters()).every(f => f !== 'none')).toBe(true);
  await page.mouse.move(0, 0);
  await expect.poll(async () => (await filters()).every(f => f === 'none')).toBe(true);
});
