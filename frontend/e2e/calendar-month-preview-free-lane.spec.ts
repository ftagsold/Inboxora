import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// Reproduces a user-reported bug: dragging an all-day event into a week that holds a multi-day
// event later in that week showed the preview one row too low, with an empty first row above it.
// The week's lane packer places longer events first and only tracks where each lane *ends*, so
// a multi-day event on Thu–Sat made lane 0 look taken on Tuesday as well. Only the dragged
// event's own preview is moved up to the lowest lane that is genuinely free on its days.
test('month view: an all-day preview takes the first free row in a week with a later multi-day event', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM layout behavior, only visible mid-drag');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [
      // Dragged: a single-day all-day event on Tue 1 September, moved one row down onto Tue 8.
      { id: 'single', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'single', all_day: true, starts_at: '2026-09-01T00:00:00Z', ends_at: '2026-09-02T00:00:00Z' },
      // Multi-day: Thu 10 – Sat 12, later in the target week; it never touches Tuesday.
      { id: 'span', calendar_id: 'calendar-personal', calendar_color: '#35793a', source: 'local', read_only: false, summary: 'span', all_day: true, starts_at: '2026-09-10T00:00:00Z', ends_at: '2026-09-13T00:00:00Z' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();

  const cells = page.locator('[data-testid="calendar-month-grid"] .cal-cell');
  const cellFor = async (label: string) => {
    for (let i = 0; i < await cells.count(); i += 1) {
      const text = (await cells.nth(i).locator('.calendar-day-select').first().textContent().catch(() => ''))?.trim();
      if (text === label) return cells.nth(i);
    }
    throw new Error(`no cell ${label}`);
  };
  const spanCell = await cellFor('10');
  const span = spanCell.locator('.cal-ev', { hasText: 'span' }).first();
  await expect(span).toBeVisible();
  const spanTop = (await span.boundingBox())!.y - (await spanCell.boundingBox())!.y;

  const event = page.locator('[data-testid="calendar-month-grid"] .cal-ev', { hasText: 'single' }).first();
  const box = (await event.boundingBox())!;
  const target = (await (await cellFor('8')).boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(target.x + target.width / 2, box.y + box.height / 2 + (target.y - (await (await cellFor('1')).boundingBox())!.y), { steps: 10 });

  const targetCell = await cellFor('8');
  const preview = targetCell.locator('[data-testid="calendar-month-move-preview"]').first();
  await expect(preview).toBeVisible();
  const previewTop = (await preview.boundingBox())!.y - (await targetCell.boundingBox())!.y;
  await page.mouse.up();

  // Same first row as the multi-day event occupies in its own cells — no empty row above it.
  expect(Math.round(previewTop)).toBe(Math.round(spanTop));
});
